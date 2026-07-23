//! Render and validate txgen-tempo workloads for the L1 -> Zone -> L1 benchmark.
//!
//! This command is deliberately configuration-only. It reads chain state and writes
//! transaction-generator inputs, but it never submits transactions or waits for bridge events.

use alloy::{
    eips::BlockNumberOrTag,
    primitives::{Address, U256},
    providers::{Provider, ProviderBuilder},
    signers::local::{MnemonicBuilder, PrivateKeySigner},
};
use eyre::{Context as _, ensure, eyre};
use serde::Serialize;
use serde_yaml::Value;
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};
use tempo_alloy::TempoNetwork;
use tempo_chainspec::spec::TEMPO_T0_BASE_FEE;
use tempo_contracts::precompiles::ITIP20;
use tempo_primitives::transaction::calc_gas_balance_spending;
use tempo_zone_contracts::{
    IZoneOutbox, ZONE_CONFIG_ADDRESS, ZONE_INBOX_ADDRESS, ZONE_OUTBOX_ADDRESS, ZonePortal,
};
use zone_primitives::constants::zone_chain_id as derive_zone_chain_id;
use zone_rpc::{ZoneProvider, ZoneProviderConfig};

use crate::zone_utils::ZoneMetadata;

const SOURCE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../contrib/bench/txgen");
const MAX_UINT256: &str =
    "115792089237316195423570985008687907853269984665640564039457584007913129639935";
const AUTH_TOKEN_TTL_SECS: u64 = 300;
alloy::sol! {
    #[sol(rpc)]
    interface ZoneBenchmarkConfig {
        function tempoPortal() external view returns (address);
        function isEnabledToken(address token) external view returns (bool);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum CheckPhase {
    Bootstrap,
    Deposit,
    Activity,
    Withdrawal,
    Roundtrip,
    All,
}

/// Expected bridge state restored before the selected benchmark phase.
///
/// This is deliberately a read-only assertion. It does not prepare either fixture or wait for
/// one to become ready.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum FixtureState {
    /// A newly created Zone with no deposits or benchmark-account Zone balances.
    Empty,
    /// The control-to-sequencer bootstrap deposit has reached the Zone. Benchmark users may
    /// still have zero Zone balances because their deposits are part of the measured journey.
    Ready,
    /// A Zone funded through deposits that have all been confirmed by an L1 batch.
    Funded,
}

impl CheckPhase {
    const fn deposit(self) -> bool {
        matches!(
            self,
            Self::Bootstrap | Self::Deposit | Self::Roundtrip | Self::All
        )
    }

    const fn withdrawal(self) -> bool {
        matches!(self, Self::Withdrawal | Self::Roundtrip | Self::All)
    }

    const fn roundtrip(self) -> bool {
        matches!(self, Self::Roundtrip)
    }
}

#[derive(Debug, clap::Parser)]
pub(crate) struct BenchmarkPreflight {
    /// Tempo L1 RPC URL. No public endpoint is selected implicitly.
    #[arg(long, env = "L1_RPC_URL")]
    l1_rpc_url: String,

    /// Zone HTTP RPC URL used for chain discovery and caller-scoped reads.
    #[arg(long, env = "ZONE_RPC_URL")]
    zone_rpc_url: String,

    /// First BIP-44 account index in the benchmark pool.
    #[arg(long, default_value_t = 0)]
    account_start: u32,

    /// Number of benchmark accounts.
    #[arg(long)]
    accounts: u32,

    /// BIP-44 account index used only for the untimed sequencer-funding deposit.
    #[arg(long, default_value_t = 0)]
    control_account_index: u32,

    /// BIP-44 account index that must resolve to the Zone's configured sequencer.
    #[arg(long, default_value_t = 4)]
    sequencer_account_index: u32,

    /// Enabled TIP-20 used for transfers and transaction fees on both networks.
    /// Falls back to initialToken in --zone-dir/zone.json.
    #[arg(long, env = "ZONES_BENCH_TOKEN")]
    token: Option<Address>,

    /// Optional generated Zone directory containing zone.json.
    #[arg(long)]
    zone_dir: Option<PathBuf>,

    /// Optional expected portal address. The Zone config remains authoritative.
    #[arg(long, env = "L1_PORTAL_ADDRESS")]
    portal: Option<Address>,

    /// Optional expected Tempo L1 chain ID. The RPC value remains authoritative.
    #[arg(long, env = "ZONES_BENCH_EXPECTED_L1_CHAIN_ID")]
    expected_l1_chain_id: Option<u64>,

    /// Optional expected Zone chain ID. The RPC value remains authoritative.
    #[arg(long, env = "ZONES_BENCH_EXPECTED_ZONE_CHAIN_ID")]
    expected_zone_chain_id: Option<u64>,

    /// Optional expected on-chain Zone ID for the resolved portal.
    #[arg(long, env = "ZONES_BENCH_EXPECTED_ZONE_ID")]
    expected_zone_id: Option<u32>,

    /// Gross amount passed to ZonePortal.deposit (includes the deposit protocol fee).
    #[arg(long)]
    deposit_amount: u128,

    /// Amount transferred by each ordinary Zone TIP-20 activity transaction.
    #[arg(long)]
    activity_amount: u128,

    /// Net amount returned to Tempo by each Zone withdrawal request.
    #[arg(long)]
    withdrawal_amount: u128,

    /// Gross control-account deposit used to fund fee configuration and sponsored approvals.
    #[arg(long, default_value_t = 10_000_000)]
    bootstrap_deposit_amount: u128,

    /// Capacity to require per account for each selected benchmark phase.
    #[arg(long, default_value_t = 1)]
    transactions_per_account: u64,

    /// Number of untimed, sponsored Zone approval rounds to fund during bootstrap.
    #[arg(long, default_value_t = 1)]
    sponsored_approval_rounds: u64,

    /// Balance/allowance checks to enforce. All networks are still queried and reported.
    #[arg(long)]
    check_phase: CheckPhase,

    /// Assert the restored bridge fixture is empty, ready, or funded for the selected phase.
    /// This only reads state; it never prepares or waits for a fixture.
    #[arg(long)]
    fixture_state: Option<FixtureState>,

    /// Do not inject untimed max-approval setup transactions when allowances are insufficient.
    #[arg(long)]
    no_approval_setup: bool,

    /// Override the queried L1 gas price used as maxFeePerGas.
    #[arg(long)]
    l1_max_fee_per_gas: Option<u128>,

    /// Override L1 maxPriorityFeePerGas (defaults to maxFeePerGas).
    #[arg(long)]
    l1_max_priority_fee_per_gas: Option<u128>,

    /// Override the queried Zone gas price used as maxFeePerGas.
    #[arg(long)]
    zone_max_fee_per_gas: Option<u128>,

    /// Override Zone maxPriorityFeePerGas (defaults to maxFeePerGas).
    #[arg(long)]
    zone_max_priority_fee_per_gas: Option<u128>,

    /// Gas limit for L1 deposit transactions.
    #[arg(long, default_value_t = 2_000_000)]
    deposit_gas_limit: u64,

    /// Gas limit for ordinary Zone TIP-20 transfer transactions.
    #[arg(long, default_value_t = 500_000)]
    activity_gas_limit: u64,

    /// Gas limit for Zone withdrawal-request transactions (not callback gasLimit).
    #[arg(long, default_value_t = 10_000_000)]
    withdrawal_tx_gas_limit: u64,

    /// Gas limit for untimed TIP-20 approval setup transactions.
    #[arg(long, default_value_t = 2_000_000)]
    approval_gas_limit: u64,

    /// Conservative Zone gas budget for the sequencer's untimed fee-configuration transaction.
    #[arg(long, default_value_t = 2_000_000)]
    fee_config_gas_limit: u64,

    /// Directory for rendered specs, copied minimal ABIs, and preflight.json.
    #[arg(long, default_value = "target/zones-benchmark")]
    output: PathBuf,
}

#[derive(Debug)]
struct AccountState {
    index: u32,
    address: Address,
    l1_balance: U256,
    portal_allowance: U256,
    zone_balance: U256,
    outbox_allowance: U256,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AccountReport {
    index: u32,
    address: String,
    l1_balance: String,
    portal_allowance: String,
    zone_balance: String,
    outbox_allowance: String,
}

impl From<&AccountState> for AccountReport {
    fn from(value: &AccountState) -> Self {
        Self {
            index: value.index,
            address: value.address.to_string(),
            l1_balance: value.l1_balance.to_string(),
            portal_allowance: value.portal_allowance.to_string(),
            zone_balance: value.zone_balance.to_string(),
            outbox_allowance: value.outbox_allowance.to_string(),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PreflightReport {
    check_phase: CheckPhase,
    fixture_state: Option<FixtureState>,
    l1_chain_id: u64,
    zone_chain_id: u64,
    l1_client_version: String,
    zone_client_version: String,
    l1_genesis_hash: String,
    zone_genesis_hash: String,
    zone_id: u32,
    portal: String,
    outbox: String,
    token: String,
    deposit_fee: u128,
    bounceback_fee: u128,
    withdrawal_fee: u128,
    portal_token_balance: String,
    deposit_count: u64,
    last_processed_deposit_number: u64,
    queried_l1_gas_price: u128,
    queried_zone_gas_price: u128,
    l1_max_fee_per_gas: u128,
    zone_max_fee_per_gas: u128,
    approval_fee_bump: u128,
    activity_fee_bump: u128,
    activity_max_fee_per_gas: u128,
    transactions_per_account: u64,
    sponsored_approval_rounds: u64,
    bootstrap_deposit_amount: u128,
    bootstrap_minimum_deposit_amount: String,
    sponsored_approval_fee_required: String,
    portal_approval_setup_accounts: Vec<u32>,
    outbox_approval_setup_accounts: Vec<u32>,
    control_account: AccountReport,
    sequencer_account: AccountReport,
    accounts: Vec<AccountReport>,
}

#[derive(Debug, Clone)]
struct RenderConfig {
    l1_chain_id: u64,
    zone_chain_id: u64,
    account_start: u32,
    account_end: u32,
    control_account_index: u32,
    sequencer_account_index: u32,
    sequencer: Address,
    portal: Address,
    outbox: Address,
    token: Address,
    deposit_amount: u128,
    activity_amount: u128,
    withdrawal_amount: u128,
    bootstrap_deposit_amount: u128,
    l1_max_fee_per_gas: u128,
    l1_max_priority_fee_per_gas: u128,
    zone_max_fee_per_gas: u128,
    zone_max_priority_fee_per_gas: u128,
    deposit_gas_limit: u64,
    activity_gas_limit: u64,
    withdrawal_tx_gas_limit: u64,
    approval_gas_limit: u64,
}

impl BenchmarkPreflight {
    pub(crate) async fn run(self) -> eyre::Result<()> {
        ensure!(self.accounts > 0, "--accounts must be greater than zero");
        ensure!(
            self.transactions_per_account > 0,
            "--transactions-per-account must be greater than zero"
        );
        ensure!(
            self.sponsored_approval_rounds > 0,
            "--sponsored-approval-rounds must be greater than zero"
        );
        ensure!(
            self.deposit_amount > 0,
            "--deposit-amount must be greater than zero"
        );
        ensure!(
            self.activity_amount > 0,
            "--activity-amount must be greater than zero"
        );
        ensure!(
            self.withdrawal_amount > 0,
            "--withdrawal-amount must be greater than zero"
        );
        ensure!(
            self.bootstrap_deposit_amount > 0,
            "--bootstrap-deposit-amount must be greater than zero"
        );
        ensure!(
            self.control_account_index != self.sequencer_account_index,
            "control and sequencer account indices must be different"
        );
        ensure!(
            self.control_account_index < u32::MAX,
            "control account index must leave room for a one-account txgen range"
        );
        ensure!(
            self.sequencer_account_index < u32::MAX,
            "sequencer account index must leave room for a one-account txgen range"
        );

        let account_end = self
            .account_start
            .checked_add(self.accounts)
            .ok_or_else(|| eyre!("benchmark account range overflows u32"))?;
        ensure!(
            !(self.account_start..account_end).contains(&self.control_account_index),
            "control account index {} overlaps the benchmark pool",
            self.control_account_index
        );
        ensure!(
            !(self.account_start..account_end).contains(&self.sequencer_account_index),
            "sequencer account index {} overlaps the benchmark pool",
            self.sequencer_account_index
        );
        // Read the mnemonic from a private file when configured so it cannot appear in command
        // line process listings. It is never written to the rendered specs or report.
        let mnemonic = read_benchmark_mnemonic()?;
        let signers = derive_signers(&mnemonic, self.account_start, account_end)?;
        let control_signer = derive_signer(&mnemonic, self.control_account_index)?;
        let sequencer_signer = derive_signer(&mnemonic, self.sequencer_account_index)?;
        let sequencer_address = sequencer_signer.address();
        let first_signer = signers
            .first()
            .cloned()
            .ok_or_else(|| eyre!("benchmark account pool is empty"))?;
        let first_address = first_signer.address();

        let metadata = self
            .zone_dir
            .as_deref()
            .map(ZoneMetadata::load)
            .transpose()?;
        let metadata_token = metadata
            .as_ref()
            .map(|value| value.get_optional_address("initialToken"))
            .transpose()?
            .flatten();
        let token =
            resolve_optional_address("token", self.token, metadata_token)?.ok_or_else(|| {
                eyre!("set --token/ZONES_BENCH_TOKEN or provide --zone-dir with initialToken")
            })?;

        let l1 = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect(&self.l1_rpc_url)
            .await
            .wrap_err("failed connecting to Tempo L1 RPC")?;
        let zone_public = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect(&self.zone_rpc_url)
            .await
            .wrap_err("failed connecting to Zone RPC")?;

        // Both IDs are read from their RPCs. In particular, the Zone ID is never derived from
        // a portal ID or copied from zone.json.
        let (
            l1_chain_id,
            zone_chain_id,
            l1_client_version,
            zone_client_version,
            l1_genesis,
            zone_genesis,
        ) = tokio::try_join!(
            l1.get_chain_id(),
            zone_public.get_chain_id(),
            l1.get_client_version(),
            zone_public.get_client_version(),
            l1.get_block_by_number(BlockNumberOrTag::Earliest),
            zone_public.get_block_by_number(BlockNumberOrTag::Earliest),
        )
        .wrap_err("failed querying L1 and Zone identities")?;
        let l1_genesis_hash = l1_genesis
            .ok_or_else(|| eyre!("Tempo L1 RPC returned no genesis block"))?
            .header
            .hash;
        let zone_genesis_hash = zone_genesis
            .ok_or_else(|| eyre!("Zone RPC returned no genesis block"))?
            .header
            .hash;
        if let Some(expected) = self.expected_l1_chain_id {
            ensure!(
                l1_chain_id == expected,
                "Tempo L1 RPC returned chain ID {l1_chain_id}, expected {expected}"
            );
        }
        if let Some(expected) = self.expected_zone_chain_id {
            ensure!(
                zone_chain_id == expected,
                "Zone RPC returned chain ID {zone_chain_id}, expected {expected}"
            );
        }

        let zone_rpc_url: url::Url = self
            .zone_rpc_url
            .parse()
            .wrap_err("Zone RPC URL must be a valid HTTP(S) URL")?;
        // Resolve the portal through the unauthenticated/internal Zone endpoint. A private Zone
        // endpoint cannot be used for discovery because its auth token already requires zoneId.
        let zone_config = ZoneBenchmarkConfig::new(ZONE_CONFIG_ADDRESS, &zone_public);
        let portal = zone_config
            .tempoPortal()
            .from(first_address)
            .call()
            .await
            .wrap_err("failed resolving ZonePortal from ZoneConfig")?;
        ensure!(
            portal != Address::ZERO,
            "ZoneConfig returned the zero ZonePortal address"
        );

        if let Some(expected) = self.portal {
            ensure!(
                expected == portal,
                "configured portal {expected} does not match ZoneConfig portal {portal}"
            );
        }
        if let Some(expected) = metadata
            .as_ref()
            .map(|value| value.get_optional_address("portal"))
            .transpose()?
            .flatten()
        {
            ensure!(
                expected == portal,
                "zone.json portal {expected} does not match ZoneConfig portal {portal}"
            );
        }
        if let Some(expected) = metadata_token {
            ensure!(
                expected == token,
                "configured token {token} does not match zone.json initialToken {expected}"
            );
        }

        let portal_contract = ZonePortal::new(portal, &l1);
        let zone_id_call = portal_contract.zoneId();
        let sequencer_call = portal_contract.isSequencer(sequencer_address);
        let (zone_id, sequencer_active) =
            tokio::try_join!(zone_id_call.call(), sequencer_call.call())
                .wrap_err("failed querying portal identity")?;
        ensure!(
            sequencer_active,
            "sequencer account index {} resolves to {sequencer_address}, but portal {portal} does not recognize it as an active sequencer",
            self.sequencer_account_index
        );
        if let Some(expected) = self.expected_zone_id {
            ensure!(
                zone_id == expected,
                "portal {portal} returned Zone ID {zone_id}, expected {expected}"
            );
        }
        let derived_zone_chain_id = derive_zone_chain_id(zone_id);
        ensure!(
            zone_chain_id == derived_zone_chain_id,
            "Zone RPC chain ID {zone_chain_id} does not match chain ID {derived_zone_chain_id} derived from portal {portal} Zone ID {zone_id}"
        );

        let zone = ZoneProvider::new(ZoneProviderConfig {
            signer: first_signer,
            zone_id,
            chain_id: zone_chain_id,
            token_ttl: Duration::from_secs(AUTH_TOKEN_TTL_SECS),
            rpc_url: zone_rpc_url.clone(),
        })?;
        let zone_provider = zone.provider();
        let zone_config = ZoneBenchmarkConfig::new(ZONE_CONFIG_ADDRESS, &zone_provider);
        let outbox = ZONE_OUTBOX_ADDRESS;

        let (
            portal_code,
            l1_token_code,
            outbox_code,
            zone_token_code,
            queried_l1_gas_price,
            queried_zone_gas_price,
        ) = tokio::try_join!(
            l1.get_code_at(portal),
            l1.get_code_at(token),
            zone_provider.get_code_at(outbox),
            zone_provider.get_code_at(token),
            l1.get_gas_price(),
            zone_provider.get_gas_price(),
        )
        .wrap_err("failed querying benchmark contract code and gas prices")?;
        let (
            token_enabled_l1,
            deposits_active,
            token_enabled_zone,
            deposit_fee,
            bounceback_fee,
            withdrawal_fee,
            portal_token_balance,
            deposit_count,
            last_processed_deposit_number,
        ) = {
            let token_enabled_l1 = portal_contract.isTokenEnabled(token);
            let deposits_active = portal_contract.areDepositsActive(token);
            let token_enabled_zone = zone_config.isEnabledToken(token).from(first_address);
            let deposit_fee = portal_contract.calculateDepositFee();
            let bounceback_fee = portal_contract.calculateBouncebackFee();
            let l1_token = ITIP20::new(token, &l1);
            let portal_token_balance = l1_token.balanceOf(portal);
            let deposit_count = portal_contract.depositCount();
            let last_processed_deposit_number = portal_contract.lastProcessedDepositNumber();
            let outbox_contract = IZoneOutbox::new(outbox, &zone_provider);
            let withdrawal_fee = outbox_contract
                .calculateWithdrawalFee(0)
                .from(first_address);
            tokio::try_join!(
                token_enabled_l1.call(),
                deposits_active.call(),
                token_enabled_zone.call(),
                deposit_fee.call(),
                bounceback_fee.call(),
                withdrawal_fee.call(),
                portal_token_balance.call(),
                deposit_count.call(),
                last_processed_deposit_number.call(),
            )
            .wrap_err("failed querying benchmark contracts and fees")?
        };

        ensure!(
            !portal_code.is_empty(),
            "no L1 contract code at portal {portal}"
        );
        ensure!(!l1_token_code.is_empty(), "no L1 TIP-20 code at {token}");
        ensure!(!outbox_code.is_empty(), "no ZoneOutbox code at {outbox}");
        ensure!(
            !zone_token_code.is_empty(),
            "no Zone TIP-20 code at {token}"
        );
        ensure!(
            token_enabled_l1,
            "token {token} is not enabled on portal {portal}"
        );
        ensure!(
            deposits_active,
            "deposits for token {token} are paused on portal {portal}"
        );
        ensure!(
            token_enabled_zone,
            "token {token} has not become enabled in the Zone's finalized L1 state"
        );

        validate_protocol_amounts(
            self.deposit_amount,
            deposit_fee,
            bounceback_fee,
            self.withdrawal_amount,
            withdrawal_fee,
        )?;
        validate_protocol_amounts(
            self.bootstrap_deposit_amount,
            deposit_fee,
            bounceback_fee,
            self.withdrawal_amount,
            withdrawal_fee,
        )?;

        let l1_max_fee_per_gas = self.l1_max_fee_per_gas.unwrap_or(queried_l1_gas_price);
        let l1_max_priority_fee_per_gas = self
            .l1_max_priority_fee_per_gas
            .unwrap_or(l1_max_fee_per_gas);
        // A fresh Zone can return zero from eth_gasPrice before it has ordinary transaction
        // history. Its genesis and public transaction filler still use Tempo's T0 base fee, so
        // never render or budget a zero-fee transaction from that estimate.
        let zone_max_fee_per_gas = self
            .zone_max_fee_per_gas
            .unwrap_or_else(|| queried_zone_gas_price.max(u128::from(TEMPO_T0_BASE_FEE)));
        let zone_max_priority_fee_per_gas = self
            .zone_max_priority_fee_per_gas
            .unwrap_or(zone_max_fee_per_gas);
        validate_gas_prices("L1", l1_max_fee_per_gas, l1_max_priority_fee_per_gas)?;
        validate_gas_prices("Zone", zone_max_fee_per_gas, zone_max_priority_fee_per_gas)?;
        let activity_transaction_capacity = u64::from(self.accounts)
            .checked_mul(self.transactions_per_account)
            .ok_or_else(|| eyre!("activity transaction capacity overflows u64"))?;
        let approval_fee_bump = u128::from(self.accounts);
        let (l1_approval_max_fee_per_gas, _) = expiring_fee_caps(
            l1_max_fee_per_gas,
            l1_max_priority_fee_per_gas,
            approval_fee_bump,
        )?;
        let (zone_approval_max_fee_per_gas, _) = expiring_fee_caps(
            zone_max_fee_per_gas,
            zone_max_priority_fee_per_gas,
            approval_fee_bump,
        )?;
        let activity_fee_bump = u128::from(activity_transaction_capacity);
        let (activity_max_fee_per_gas, _) = expiring_fee_caps(
            zone_max_fee_per_gas,
            zone_max_priority_fee_per_gas,
            activity_fee_bump,
        )?;

        let mut states = Vec::with_capacity(signers.len());
        for (offset, signer) in signers.into_iter().enumerate() {
            let address = signer.address();
            let private_zone = ZoneProvider::new(ZoneProviderConfig {
                signer,
                zone_id,
                chain_id: zone_chain_id,
                token_ttl: Duration::from_secs(AUTH_TOKEN_TTL_SECS),
                rpc_url: zone_rpc_url.clone(),
            })?;
            let account_zone = private_zone.provider();
            let l1_token = ITIP20::new(token, &l1);
            let zone_token = ITIP20::new(token, &account_zone);
            let l1_balance = l1_token.balanceOf(address).from(address);
            let portal_allowance = l1_token.allowance(address, portal).from(address);
            let zone_balance = zone_token.balanceOf(address).from(address);
            let outbox_allowance = zone_token.allowance(address, outbox).from(address);
            let (l1_balance, portal_allowance, zone_balance, outbox_allowance) = tokio::try_join!(
                l1_balance.call(),
                portal_allowance.call(),
                zone_balance.call(),
                outbox_allowance.call(),
            )
            .wrap_err_with(|| {
                format!("failed reading balances/allowances for account {address}")
            })?;
            states.push(AccountState {
                index: self.account_start + offset as u32,
                address,
                l1_balance,
                portal_allowance,
                zone_balance,
                outbox_allowance,
            });
        }

        let mut infrastructure_states = Vec::with_capacity(2);
        for (index, signer) in [
            (self.control_account_index, control_signer),
            (self.sequencer_account_index, sequencer_signer),
        ] {
            let address = signer.address();
            let private_zone = ZoneProvider::new(ZoneProviderConfig {
                signer,
                zone_id,
                chain_id: zone_chain_id,
                token_ttl: Duration::from_secs(AUTH_TOKEN_TTL_SECS),
                rpc_url: zone_rpc_url.clone(),
            })?;
            let account_zone = private_zone.provider();
            let l1_token = ITIP20::new(token, &l1);
            let zone_token = ITIP20::new(token, &account_zone);
            let l1_balance = l1_token.balanceOf(address).from(address);
            let portal_allowance = l1_token.allowance(address, portal).from(address);
            let zone_balance = zone_token.balanceOf(address).from(address);
            let outbox_allowance = zone_token.allowance(address, outbox).from(address);
            let (l1_balance, portal_allowance, zone_balance, outbox_allowance) = tokio::try_join!(
                l1_balance.call(),
                portal_allowance.call(),
                zone_balance.call(),
                outbox_allowance.call(),
            )
            .wrap_err_with(|| {
                format!("failed reading balances/allowances for infrastructure account {address}")
            })?;
            infrastructure_states.push(AccountState {
                index,
                address,
                l1_balance,
                portal_allowance,
                zone_balance,
                outbox_allowance,
            });
        }
        let sequencer_state = infrastructure_states
            .pop()
            .expect("sequencer state was queried");
        let control_state = infrastructure_states
            .pop()
            .expect("control state was queried");

        let tx_count = U256::from(self.transactions_per_account);
        let portal_allowance_required = U256::from(self.deposit_amount)
            .checked_mul(tx_count)
            .ok_or_else(|| eyre!("deposit allowance requirement overflowed U256"))?;
        let withdrawal_debit = U256::from(self.withdrawal_amount)
            .checked_add(U256::from(withdrawal_fee))
            .ok_or_else(|| eyre!("withdrawal amount plus protocol fee overflowed U256"))?;
        let outbox_allowance_required = withdrawal_debit
            .checked_mul(tx_count)
            .ok_or_else(|| eyre!("withdrawal allowance requirement overflowed U256"))?;

        let portal_setup: Vec<usize> = states
            .iter()
            .enumerate()
            .filter_map(|(offset, state)| {
                (state.portal_allowance < portal_allowance_required).then_some(offset)
            })
            .collect();
        let outbox_setup: Vec<usize> = states
            .iter()
            .enumerate()
            .filter_map(|(offset, state)| {
                (state.outbox_allowance < outbox_allowance_required).then_some(offset)
            })
            .collect();

        let l1_control_approval_fee =
            calc_gas_balance_spending(self.approval_gas_limit, l1_max_fee_per_gas);
        let l1_deposit_tx_fee =
            calc_gas_balance_spending(self.deposit_gas_limit, l1_max_fee_per_gas);
        let zone_approval_fee =
            calc_gas_balance_spending(self.approval_gas_limit, zone_approval_max_fee_per_gas);
        let zone_fee_config_fee =
            calc_gas_balance_spending(self.fee_config_gas_limit, zone_max_fee_per_gas);
        let all_sponsored_approval_fees = zone_approval_fee
            .checked_mul(U256::from(self.accounts))
            .and_then(|value| value.checked_mul(U256::from(self.sponsored_approval_rounds)))
            .ok_or_else(|| eyre!("sponsored approval fee requirement overflowed U256"))?;
        let bootstrap_net_required = zone_fee_config_fee
            .checked_add(all_sponsored_approval_fees)
            .ok_or_else(|| eyre!("bootstrap Zone fee requirement overflowed U256"))?;
        let bootstrap_minimum_deposit_amount = U256::from(deposit_fee)
            .checked_add(bootstrap_net_required.max(U256::from(bounceback_fee)))
            .ok_or_else(|| eyre!("bootstrap deposit requirement overflowed U256"))?;
        if matches!(
            self.check_phase,
            CheckPhase::Bootstrap | CheckPhase::Roundtrip
        ) {
            ensure!(
                U256::from(self.bootstrap_deposit_amount) >= bootstrap_minimum_deposit_amount,
                "bootstrap deposit amount {} is below required {} for sequencer fee configuration and {} rounds of {} sponsored approvals",
                self.bootstrap_deposit_amount,
                bootstrap_minimum_deposit_amount,
                self.sponsored_approval_rounds,
                self.accounts
            );
        }

        let control_needs_setup =
            control_state.portal_allowance < U256::from(self.bootstrap_deposit_amount);
        if self.check_phase == CheckPhase::Bootstrap {
            if self.no_approval_setup {
                ensure!(
                    !control_needs_setup,
                    "control account {} portal allowance {} is below bootstrap amount {}; enable approval setup or approve first",
                    control_state.address,
                    control_state.portal_allowance,
                    self.bootstrap_deposit_amount
                );
            }
            let control_required = U256::from(self.bootstrap_deposit_amount)
                .checked_add(l1_deposit_tx_fee)
                .and_then(|value| {
                    value.checked_add(if control_needs_setup && !self.no_approval_setup {
                        l1_control_approval_fee
                    } else {
                        U256::ZERO
                    })
                })
                .ok_or_else(|| eyre!("control-account bootstrap requirement overflowed U256"))?;
            ensure!(
                control_state.l1_balance >= control_required,
                "control account {} L1 balance {} is below bootstrap requirement {}",
                control_state.address,
                control_state.l1_balance,
                control_required
            );
        }

        let sponsored_approval_fee_required = zone_approval_fee
            .checked_mul(U256::from(outbox_setup.len()))
            .ok_or_else(|| eyre!("selected sponsored approval fee requirement overflowed U256"))?;
        if self.check_phase.roundtrip() {
            ensure!(
                withdrawal_fee > 0,
                "roundtrip benchmark requires a nonzero Zone withdrawal fee; fund the sequencer and configure ZoneOutbox first"
            );
            ensure!(
                sequencer_state.zone_balance >= sponsored_approval_fee_required,
                "sequencer {} Zone balance {} is below required {} to sponsor {} outbox approvals",
                sequencer_state.address,
                sequencer_state.zone_balance,
                sponsored_approval_fee_required,
                outbox_setup.len()
            );
        }

        validate_account_capacity(
            &states,
            self.check_phase,
            self.no_approval_setup,
            self.transactions_per_account,
            self.deposit_amount,
            deposit_fee,
            self.activity_amount,
            self.withdrawal_amount,
            withdrawal_fee,
            portal_allowance_required,
            outbox_allowance_required,
            l1_max_fee_per_gas,
            zone_max_fee_per_gas,
            activity_max_fee_per_gas,
            l1_approval_max_fee_per_gas,
            zone_approval_max_fee_per_gas,
            self.deposit_gas_limit,
            self.activity_gas_limit,
            self.withdrawal_tx_gas_limit,
            self.approval_gas_limit,
        )?;
        if let Some(fixture_state) = self.fixture_state {
            validate_fixture_state(
                &states,
                self.check_phase,
                fixture_state,
                portal_token_balance,
                deposit_count,
                last_processed_deposit_number,
            )?;
        }

        let render = RenderConfig {
            l1_chain_id,
            zone_chain_id,
            account_start: self.account_start,
            account_end,
            control_account_index: self.control_account_index,
            sequencer_account_index: self.sequencer_account_index,
            sequencer: sequencer_address,
            portal,
            outbox,
            token,
            deposit_amount: self.deposit_amount,
            activity_amount: self.activity_amount,
            withdrawal_amount: self.withdrawal_amount,
            bootstrap_deposit_amount: self.bootstrap_deposit_amount,
            l1_max_fee_per_gas,
            l1_max_priority_fee_per_gas,
            zone_max_fee_per_gas,
            zone_max_priority_fee_per_gas,
            deposit_gas_limit: self.deposit_gas_limit,
            activity_gas_limit: self.activity_gas_limit,
            withdrawal_tx_gas_limit: self.withdrawal_tx_gas_limit,
            approval_gas_limit: self.approval_gas_limit,
        };

        let portal_setup = if self.no_approval_setup || !self.check_phase.deposit() {
            Vec::new()
        } else {
            portal_setup
        };
        let outbox_setup = if self.no_approval_setup || !self.check_phase.withdrawal() {
            Vec::new()
        } else {
            outbox_setup
        };
        render_all_specs(
            &self.output,
            &render,
            control_needs_setup && !self.no_approval_setup,
            &portal_setup,
            &outbox_setup,
        )?;

        let report = PreflightReport {
            check_phase: self.check_phase,
            fixture_state: self.fixture_state,
            l1_chain_id,
            zone_chain_id,
            l1_client_version: l1_client_version.clone(),
            zone_client_version: zone_client_version.clone(),
            l1_genesis_hash: l1_genesis_hash.to_string(),
            zone_genesis_hash: zone_genesis_hash.to_string(),
            zone_id,
            portal: portal.to_string(),
            outbox: outbox.to_string(),
            token: token.to_string(),
            deposit_fee,
            bounceback_fee,
            withdrawal_fee,
            portal_token_balance: portal_token_balance.to_string(),
            deposit_count,
            last_processed_deposit_number,
            queried_l1_gas_price,
            queried_zone_gas_price,
            l1_max_fee_per_gas,
            zone_max_fee_per_gas,
            approval_fee_bump,
            activity_fee_bump,
            activity_max_fee_per_gas,
            transactions_per_account: self.transactions_per_account,
            sponsored_approval_rounds: self.sponsored_approval_rounds,
            bootstrap_deposit_amount: self.bootstrap_deposit_amount,
            bootstrap_minimum_deposit_amount: bootstrap_minimum_deposit_amount.to_string(),
            sponsored_approval_fee_required: sponsored_approval_fee_required.to_string(),
            portal_approval_setup_accounts: portal_setup
                .iter()
                .map(|offset| self.account_start + *offset as u32)
                .collect(),
            outbox_approval_setup_accounts: outbox_setup
                .iter()
                .map(|offset| self.account_start + *offset as u32)
                .collect(),
            control_account: AccountReport::from(&control_state),
            sequencer_account: AccountReport::from(&sequencer_state),
            accounts: states.iter().map(AccountReport::from).collect(),
        };
        let report_path = self.output.join("preflight.json");
        fs::write(
            &report_path,
            serde_json::to_string_pretty(&report)
                .wrap_err("failed serializing preflight report")?,
        )
        .wrap_err_with(|| format!("failed writing {}", report_path.display()))?;

        println!(
            "Zones benchmark preflight passed for {} accounts",
            states.len()
        );
        println!("  L1 chain ID:       {l1_chain_id}");
        println!("  Zone chain ID:     {zone_chain_id}");
        println!("  L1 client:         {l1_client_version}");
        println!("  Zone client:       {zone_client_version}");
        println!("  L1 genesis:        {l1_genesis_hash}");
        println!("  Zone genesis:      {zone_genesis_hash}");
        println!("  Zone ID:           {zone_id}");
        println!("  Portal:            {portal}");
        println!("  Outbox:            {outbox}");
        println!("  Token / fee token: {token}");
        println!("  Deposit fee:       {deposit_fee}");
        println!("  Bounceback fee:    {bounceback_fee}");
        println!("  Withdrawal fee:    {withdrawal_fee}");
        println!("  Portal balance:    {portal_token_balance}");
        println!("  Deposits processed: {last_processed_deposit_number}/{deposit_count}");
        println!("  L1 gas estimate:    {queried_l1_gas_price}");
        println!("  Zone gas estimate:  {queried_zone_gas_price}");
        println!("  Zone max fee:       {zone_max_fee_per_gas}");
        println!("  Approval fee bump: {approval_fee_bump}");
        println!("  Activity fee bump: {activity_fee_bump}");
        println!(
            "  Sponsored approval rounds: {}",
            self.sponsored_approval_rounds
        );
        println!("  Bootstrap amount:  {}", self.bootstrap_deposit_amount);
        println!("  Bootstrap minimum: {bootstrap_minimum_deposit_amount}");
        println!("  Approval sponsor:  {sequencer_address}");
        println!("  Rendered specs:    {}", self.output.display());
        println!("  Account report:    {}", report_path.display());

        Ok(())
    }
}

fn read_benchmark_mnemonic() -> eyre::Result<String> {
    let mnemonic = if let Some(path) = std::env::var_os("ZONES_BENCH_MNEMONIC_FILE") {
        let path = PathBuf::from(path);
        fs::read_to_string(&path).wrap_err_with(|| {
            format!("failed reading benchmark mnemonic file {}", path.display())
        })?
    } else {
        std::env::var("ZONES_BENCH_MNEMONIC")
            .wrap_err("set ZONES_BENCH_MNEMONIC_FILE for benchmark address derivation")?
    };
    let mnemonic = mnemonic.trim().to_owned();
    ensure!(!mnemonic.is_empty(), "benchmark mnemonic is empty");
    Ok(mnemonic)
}

fn derive_signers(
    mnemonic: &str,
    account_start: u32,
    account_end: u32,
) -> eyre::Result<Vec<PrivateKeySigner>> {
    (account_start..account_end)
        .map(|index| {
            MnemonicBuilder::from_phrase(mnemonic)
                .index(index)
                .and_then(|builder| builder.build())
                .map_err(|err| eyre!("failed deriving benchmark account index {index}: {err}"))
        })
        .collect()
}

fn derive_signer(mnemonic: &str, index: u32) -> eyre::Result<PrivateKeySigner> {
    MnemonicBuilder::from_phrase(mnemonic)
        .index(index)
        .and_then(|builder| builder.build())
        .map_err(|err| {
            eyre!("failed deriving benchmark infrastructure account index {index}: {err}")
        })
}

fn resolve_optional_address(
    label: &str,
    configured: Option<Address>,
    metadata: Option<Address>,
) -> eyre::Result<Option<Address>> {
    if let (Some(configured), Some(metadata)) = (configured, metadata) {
        ensure!(
            configured == metadata,
            "configured {label} {configured} does not match zone.json {label} {metadata}"
        );
    }
    Ok(configured.or(metadata))
}

fn validate_protocol_amounts(
    deposit_amount: u128,
    deposit_fee: u128,
    bounceback_fee: u128,
    withdrawal_amount: u128,
    withdrawal_fee: u128,
) -> eyre::Result<()> {
    let deposit_minimum = deposit_fee
        .checked_add(bounceback_fee)
        .ok_or_else(|| eyre!("deposit and bounceback fees overflow u128"))?;
    ensure!(
        deposit_amount >= deposit_minimum,
        "deposit amount {deposit_amount} cannot cover deposit fee {deposit_fee} plus bounceback fee {bounceback_fee}"
    );
    withdrawal_amount
        .checked_add(withdrawal_fee)
        .ok_or_else(|| eyre!("withdrawal amount plus fee overflows uint128"))?;
    Ok(())
}

fn validate_gas_prices(label: &str, max_fee: u128, max_priority: u128) -> eyre::Result<()> {
    ensure!(
        max_fee > 0,
        "{label} maxFeePerGas must be greater than zero"
    );
    ensure!(
        max_priority <= max_fee,
        "{label} maxPriorityFeePerGas {max_priority} exceeds maxFeePerGas {max_fee}"
    );
    Ok(())
}

fn expiring_fee_caps(
    max_fee: u128,
    max_priority: u128,
    maximum_bump: u128,
) -> eyre::Result<(u128, u128)> {
    let max_fee = max_fee
        .checked_add(maximum_bump)
        .ok_or_else(|| eyre!("maxFeePerGas overflows the txgen expiring-nonce uniqueness bump"))?;
    let max_priority = max_priority.checked_add(maximum_bump).ok_or_else(|| {
        eyre!("maxPriorityFeePerGas overflows the txgen expiring-nonce uniqueness bump")
    })?;
    Ok((max_fee, max_priority))
}

#[allow(clippy::too_many_arguments)]
fn validate_account_capacity(
    states: &[AccountState],
    phase: CheckPhase,
    no_approval_setup: bool,
    transactions_per_account: u64,
    deposit_amount: u128,
    deposit_fee: u128,
    activity_amount: u128,
    withdrawal_amount: u128,
    withdrawal_fee: u128,
    portal_allowance_required: U256,
    outbox_allowance_required: U256,
    l1_max_fee_per_gas: u128,
    zone_max_fee_per_gas: u128,
    zone_activity_max_fee_per_gas: u128,
    l1_approval_max_fee_per_gas: u128,
    zone_approval_max_fee_per_gas: u128,
    deposit_gas_limit: u64,
    activity_gas_limit: u64,
    withdrawal_tx_gas_limit: u64,
    approval_gas_limit: u64,
) -> eyre::Result<()> {
    let count = U256::from(transactions_per_account);
    let l1_tx_fee = calc_gas_balance_spending(deposit_gas_limit, l1_max_fee_per_gas);
    let zone_activity_tx_fee =
        calc_gas_balance_spending(activity_gas_limit, zone_activity_max_fee_per_gas);
    let zone_withdrawal_tx_fee =
        calc_gas_balance_spending(withdrawal_tx_gas_limit, zone_max_fee_per_gas);
    let l1_approval_fee =
        calc_gas_balance_spending(approval_gas_limit, l1_approval_max_fee_per_gas);
    let zone_approval_fee =
        calc_gas_balance_spending(approval_gas_limit, zone_approval_max_fee_per_gas);

    if phase.roundtrip() {
        let deposit_net = U256::from(
            deposit_amount
                .checked_sub(deposit_fee)
                .ok_or_else(|| eyre!("roundtrip deposit amount is below its protocol fee"))?,
        );
        let journey_required = U256::from(activity_amount)
            .checked_add(zone_activity_tx_fee)
            .and_then(|value| value.checked_add(U256::from(withdrawal_amount)))
            .and_then(|value| value.checked_add(U256::from(withdrawal_fee)))
            .and_then(|value| value.checked_add(zone_withdrawal_tx_fee))
            .ok_or_else(|| eyre!("roundtrip per-journey Zone requirement overflowed U256"))?;
        ensure!(
            deposit_net >= journey_required,
            "roundtrip deposit credits {} Zone token units, below required {} for activity, withdrawal, protocol fee, and transaction fee caps",
            deposit_net,
            journey_required
        );
    }

    for state in states {
        let portal_needs_setup = state.portal_allowance < portal_allowance_required;
        let outbox_needs_setup = state.outbox_allowance < outbox_allowance_required;

        if phase.deposit() && no_approval_setup {
            ensure!(
                !portal_needs_setup,
                "account {} portal allowance {} is below required {}; enable approval setup or approve first",
                state.address,
                state.portal_allowance,
                portal_allowance_required
            );
        }
        if phase.withdrawal() && no_approval_setup {
            ensure!(
                !outbox_needs_setup,
                "account {} outbox allowance {} is below required {}; enable approval setup or approve first",
                state.address,
                state.outbox_allowance,
                outbox_allowance_required
            );
        }

        if phase.deposit() {
            let required = U256::from(deposit_amount)
                .checked_add(l1_tx_fee)
                .and_then(|value| value.checked_mul(count))
                .and_then(|value| {
                    value.checked_add(if portal_needs_setup && !no_approval_setup {
                        l1_approval_fee
                    } else {
                        U256::ZERO
                    })
                })
                .ok_or_else(|| eyre!("L1 balance requirement overflowed U256"))?;
            ensure!(
                state.l1_balance >= required,
                "account {} L1 balance {} is below required {} for the selected deposit capacity",
                state.address,
                state.l1_balance,
                required
            );
        }

        let activity_required = U256::from(activity_amount)
            .checked_add(zone_activity_tx_fee)
            .and_then(|value| value.checked_mul(count))
            .ok_or_else(|| eyre!("Zone activity balance requirement overflowed U256"))?;
        let withdrawal_required = U256::from(withdrawal_amount)
            .checked_add(U256::from(withdrawal_fee))
            .and_then(|value| value.checked_add(zone_withdrawal_tx_fee))
            .and_then(|value| value.checked_mul(count))
            .and_then(|value| {
                value.checked_add(if outbox_needs_setup && !no_approval_setup {
                    zone_approval_fee
                } else {
                    U256::ZERO
                })
            })
            .ok_or_else(|| eyre!("Zone withdrawal balance requirement overflowed U256"))?;
        let zone_required = match phase {
            CheckPhase::Bootstrap | CheckPhase::Deposit | CheckPhase::Roundtrip => None,
            CheckPhase::Activity => Some(activity_required),
            CheckPhase::Withdrawal => Some(withdrawal_required),
            CheckPhase::All => Some(
                activity_required
                    .checked_add(withdrawal_required)
                    .ok_or_else(|| eyre!("combined Zone balance requirement overflowed U256"))?,
            ),
        };
        if let Some(required) = zone_required {
            ensure!(
                state.zone_balance >= required,
                "account {} Zone balance {} is below required {} for the selected phase capacity",
                state.address,
                state.zone_balance,
                required
            );
        }
    }
    Ok(())
}

fn validate_fixture_state(
    states: &[AccountState],
    phase: CheckPhase,
    fixture_state: FixtureState,
    portal_token_balance: U256,
    deposit_count: u64,
    last_processed_deposit_number: u64,
) -> eyre::Result<()> {
    match fixture_state {
        FixtureState::Empty => {
            ensure!(
                matches!(phase, CheckPhase::Bootstrap | CheckPhase::Deposit),
                "--fixture-state empty is only valid with --check-phase bootstrap or deposit"
            );
            ensure!(
                deposit_count == 0,
                "empty fixture portal has recorded {deposit_count} deposits"
            );
            ensure!(
                last_processed_deposit_number == 0,
                "empty fixture portal has processed {last_processed_deposit_number} deposits"
            );
            ensure!(
                portal_token_balance.is_zero(),
                "empty fixture portal holds token balance {portal_token_balance}"
            );
            for state in states {
                ensure!(
                    state.zone_balance.is_zero(),
                    "empty fixture account {} (index {}) has Zone balance {}",
                    state.address,
                    state.index,
                    state.zone_balance
                );
            }
        }
        FixtureState::Ready => {
            ensure!(
                phase == CheckPhase::Roundtrip,
                "--fixture-state ready requires --check-phase roundtrip"
            );
            ensure!(
                deposit_count > 0,
                "ready fixture portal has no bootstrap deposit"
            );
            ensure!(
                last_processed_deposit_number == deposit_count,
                "ready fixture has only processed {last_processed_deposit_number} of {deposit_count} recorded deposits"
            );
            ensure!(
                !portal_token_balance.is_zero(),
                "ready fixture portal has no token backing for the sequencer bootstrap"
            );
            for state in states {
                ensure!(
                    state.zone_balance.is_zero(),
                    "ready fixture benchmark account {} (index {}) already has Zone balance {}; measured users must start unfunded",
                    state.address,
                    state.index,
                    state.zone_balance
                );
            }
        }
        FixtureState::Funded => {
            ensure!(
                matches!(
                    phase,
                    CheckPhase::Activity | CheckPhase::Withdrawal | CheckPhase::All
                ),
                "--fixture-state funded requires --check-phase activity, withdrawal, or all"
            );
            ensure!(
                deposit_count > 0,
                "funded fixture portal has no recorded deposits"
            );
            ensure!(
                last_processed_deposit_number == deposit_count,
                "funded fixture has only processed {last_processed_deposit_number} of {deposit_count} recorded deposits"
            );
            let pool_zone_balance = states.iter().try_fold(U256::ZERO, |total, state| {
                total
                    .checked_add(state.zone_balance)
                    .ok_or_else(|| eyre!("benchmark pool Zone balance overflowed U256"))
            })?;
            ensure!(
                !pool_zone_balance.is_zero(),
                "funded fixture benchmark pool has zero aggregate Zone balance"
            );
            ensure!(
                portal_token_balance >= pool_zone_balance,
                "funded fixture portal balance {portal_token_balance} does not back benchmark pool Zone balance {pool_zone_balance}"
            );
        }
    }
    Ok(())
}

fn render_all_specs(
    output: &Path,
    config: &RenderConfig,
    control_needs_setup: bool,
    portal_setup: &[usize],
    outbox_setup: &[usize],
) -> eyre::Result<()> {
    fs::create_dir_all(output).wrap_err_with(|| format!("failed creating {}", output.display()))?;
    let output_abis = output.join("abis");
    fs::create_dir_all(&output_abis)
        .wrap_err_with(|| format!("failed creating {}", output_abis.display()))?;
    for name in [
        "tip20.json",
        "zone-inbox.json",
        "zone-portal.json",
        "zone-outbox.json",
    ] {
        let source = Path::new(SOURCE_DIR).join("abis").join(name);
        let destination = output_abis.join(name);
        fs::copy(&source, &destination).wrap_err_with(|| {
            format!(
                "failed copying benchmark ABI {} to {}",
                source.display(),
                destination.display()
            )
        })?;
    }

    let common = common_replacements(config);
    let deposit_steps = portal_setup
        .iter()
        .map(|offset| {
            approval_step(
                "users",
                *offset,
                config.account_start + *offset as u32,
                config,
                config.portal,
                ApprovalOptions {
                    l1: true,
                    sponsored: false,
                },
            )
        })
        .collect::<eyre::Result<Vec<_>>>()?;
    let withdrawal_steps = outbox_setup
        .iter()
        .map(|offset| {
            approval_step(
                "users",
                *offset,
                config.account_start + *offset as u32,
                config,
                config.outbox,
                ApprovalOptions {
                    l1: false,
                    sponsored: false,
                },
            )
        })
        .collect::<eyre::Result<Vec<_>>>()?;
    let bootstrap_steps = control_needs_setup
        .then(|| {
            approval_step(
                "control",
                0,
                config.control_account_index,
                config,
                config.portal,
                ApprovalOptions {
                    l1: true,
                    sponsored: false,
                },
            )
        })
        .transpose()?
        .into_iter()
        .collect();

    render_spec(
        "deposit.yml",
        &output.join("deposit.yml"),
        &common,
        deposit_steps,
    )?;
    render_spec(
        "zone-activity.yml",
        &output.join("zone-activity.yml"),
        &common,
        Vec::new(),
    )?;
    render_spec(
        "withdrawal.yml",
        &output.join("withdrawal.yml"),
        &common,
        withdrawal_steps,
    )?;
    render_spec(
        "bootstrap-deposit.yml",
        &output.join("bootstrap-deposit.yml"),
        &common,
        bootstrap_steps,
    )?;
    render_spec(
        "zone-roundtrip.yml",
        &output.join("zone-roundtrip.yml"),
        &common,
        outbox_setup
            .iter()
            .map(|offset| {
                approval_step(
                    "users",
                    *offset,
                    config.account_start + *offset as u32,
                    config,
                    config.outbox,
                    ApprovalOptions {
                        l1: false,
                        sponsored: true,
                    },
                )
            })
            .collect::<eyre::Result<Vec<_>>>()?,
    )?;
    render_document(
        "scenario-fragments.yml",
        &output.join("scenario-fragments.yml"),
        &common,
        false,
    )?;
    render_document(
        "bootstrap-scenario.yml",
        &output.join("bootstrap-scenario.yml"),
        &common,
        false,
    )?;
    render_document(
        "roundtrip-scenario.yml",
        &output.join("roundtrip-scenario.yml"),
        &common,
        false,
    )?;
    Ok(())
}

fn common_replacements(config: &RenderConfig) -> HashMap<String, Value> {
    HashMap::from([
        ("__L1_CHAIN_ID__".into(), Value::from(config.l1_chain_id)),
        (
            "__ZONE_CHAIN_ID__".into(),
            Value::from(config.zone_chain_id),
        ),
        (
            "__ACCOUNT_START__".into(),
            Value::from(config.account_start),
        ),
        ("__ACCOUNT_END__".into(), Value::from(config.account_end)),
        (
            "__CONTROL_ACCOUNT_INDEX__".into(),
            Value::from(config.control_account_index),
        ),
        (
            "__CONTROL_ACCOUNT_END__".into(),
            Value::from(config.control_account_index + 1),
        ),
        (
            "__SEQUENCER_ACCOUNT_INDEX__".into(),
            Value::from(config.sequencer_account_index),
        ),
        (
            "__SEQUENCER_ACCOUNT_END__".into(),
            Value::from(config.sequencer_account_index + 1),
        ),
        (
            "__SEQUENCER__".into(),
            Value::from(config.sequencer.to_string()),
        ),
        ("__PORTAL__".into(), Value::from(config.portal.to_string())),
        (
            "__INBOX__".into(),
            Value::from(ZONE_INBOX_ADDRESS.to_string()),
        ),
        ("__OUTBOX__".into(), Value::from(config.outbox.to_string())),
        ("__TOKEN__".into(), Value::from(config.token.to_string())),
        (
            "__DEPOSIT_AMOUNT__".into(),
            yaml_value(config.deposit_amount),
        ),
        (
            "__ACTIVITY_AMOUNT__".into(),
            yaml_value(config.activity_amount),
        ),
        (
            "__WITHDRAWAL_AMOUNT__".into(),
            yaml_value(config.withdrawal_amount),
        ),
        (
            "__BOOTSTRAP_DEPOSIT_AMOUNT__".into(),
            yaml_value(config.bootstrap_deposit_amount),
        ),
        (
            "__L1_MAX_FEE_PER_GAS__".into(),
            yaml_value(config.l1_max_fee_per_gas),
        ),
        (
            "__L1_MAX_PRIORITY_FEE_PER_GAS__".into(),
            yaml_value(config.l1_max_priority_fee_per_gas),
        ),
        (
            "__ZONE_MAX_FEE_PER_GAS__".into(),
            yaml_value(config.zone_max_fee_per_gas),
        ),
        (
            "__ZONE_MAX_PRIORITY_FEE_PER_GAS__".into(),
            yaml_value(config.zone_max_priority_fee_per_gas),
        ),
        (
            "__DEPOSIT_GAS_LIMIT__".into(),
            Value::from(config.deposit_gas_limit),
        ),
        (
            "__ACTIVITY_GAS_LIMIT__".into(),
            Value::from(config.activity_gas_limit),
        ),
        (
            "__WITHDRAWAL_TX_GAS_LIMIT__".into(),
            Value::from(config.withdrawal_tx_gas_limit),
        ),
    ])
}

#[derive(Clone, Copy)]
struct ApprovalOptions {
    l1: bool,
    sponsored: bool,
}

fn approval_step(
    pool: &str,
    pool_offset: usize,
    account_index: u32,
    config: &RenderConfig,
    spender: Address,
    options: ApprovalOptions,
) -> eyre::Result<Value> {
    let (max_fee, max_priority) = if options.l1 {
        (
            config.l1_max_fee_per_gas,
            config.l1_max_priority_fee_per_gas,
        )
    } else {
        (
            config.zone_max_fee_per_gas,
            config.zone_max_priority_fee_per_gas,
        )
    };
    let mut transaction = serde_json::json!({
        "type": "tempo",
        "from": {
            "pool": pool,
            "select": { "index": pool_offset },
        },
        "gas_limit": config.approval_gas_limit,
        "max_fee_per_gas": max_fee,
        "max_priority_fee_per_gas": max_priority,
        "fee_token": config.token.to_string(),
        "call": {
            "to": config.token.to_string(),
            "abi": "TIP20",
            "function": "approve",
            "args": [spender.to_string(), MAX_UINT256],
        },
    });
    if options.sponsored {
        transaction["sponsor"] = serde_json::json!({
            "pool": "sponsor",
            "select": { "index": 0 },
        });
    }
    serde_yaml::to_value(serde_json::json!({
        "id": format!(
            "approve_{}_account_{account_index}",
            if options.l1 { "portal" } else { "outbox" }
        ),
        "tx": transaction,
    }))
    .wrap_err("failed encoding approval setup step")
}

fn render_spec(
    source_name: &str,
    destination: &Path,
    replacements: &HashMap<String, Value>,
    setup_steps: Vec<Value>,
) -> eyre::Result<()> {
    let source = Path::new(SOURCE_DIR).join(source_name);
    let contents = fs::read_to_string(&source)
        .wrap_err_with(|| format!("failed reading {}", source.display()))?;
    let mut document: Value = serde_yaml::from_str(&contents)
        .wrap_err_with(|| format!("failed parsing {}", source.display()))?;
    replace_placeholders(&mut document, replacements);
    let setup = document
        .get_mut("setup")
        .and_then(Value::as_mapping_mut)
        .ok_or_else(|| eyre!("{} must contain a setup mapping", source.display()))?;
    setup.insert(Value::from("steps"), Value::Sequence(setup_steps));

    write_rendered_document(source, destination, document, true)
}

fn render_document(
    source_name: &str,
    destination: &Path,
    replacements: &HashMap<String, Value>,
    requires_mnemonic: bool,
) -> eyre::Result<()> {
    let source = Path::new(SOURCE_DIR).join(source_name);
    let contents = fs::read_to_string(&source)
        .wrap_err_with(|| format!("failed reading {}", source.display()))?;
    let mut document: Value = serde_yaml::from_str(&contents)
        .wrap_err_with(|| format!("failed parsing {}", source.display()))?;
    replace_placeholders(&mut document, replacements);
    write_rendered_document(source, destination, document, requires_mnemonic)
}

fn write_rendered_document(
    source: PathBuf,
    destination: &Path,
    document: Value,
    requires_mnemonic: bool,
) -> eyre::Result<()> {
    let rendered = serde_yaml::to_string(&document)
        .wrap_err_with(|| format!("failed rendering {}", source.display()))?;
    ensure!(
        !rendered.contains("__"),
        "unresolved renderer placeholder in {}",
        source.display()
    );
    if requires_mnemonic {
        ensure!(
            rendered.contains("${ZONES_BENCH_MNEMONIC}"),
            "{} must retain the runtime mnemonic environment reference",
            source.display()
        );
    }
    fs::write(destination, rendered)
        .wrap_err_with(|| format!("failed writing {}", destination.display()))
}

fn replace_placeholders(value: &mut Value, replacements: &HashMap<String, Value>) {
    match value {
        Value::String(current) => {
            if let Some(replacement) = replacements.get(current) {
                *value = replacement.clone();
            }
        }
        Value::Sequence(values) => {
            for value in values {
                replace_placeholders(value, replacements);
            }
        }
        Value::Mapping(values) => {
            for value in values.values_mut() {
                replace_placeholders(value, replacements);
            }
        }
        Value::Tagged(tagged) => replace_placeholders(&mut tagged.value, replacements),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn yaml_value(value: impl Serialize) -> Value {
    serde_yaml::to_value(value).expect("primitive benchmark value must serialize to YAML")
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::{
        consensus::{Transaction, transaction::SignerRecoverable},
        eips::eip2718::Decodable2718,
        primitives::{TxKind, address},
        sol_types::SolCall,
    };
    use std::{
        process::Command,
        time::{SystemTime, UNIX_EPOCH},
    };
    use tempo_primitives::TempoTxEnvelope;

    const TEST_MNEMONIC: &str = "test test test test test test test test test test test junk";

    #[test]
    fn protocol_amounts_cover_bridge_fees() {
        validate_protocol_amounts(30, 10, 20, u128::MAX - 2, 2).unwrap();
        assert!(validate_protocol_amounts(29, 10, 20, 1, 1).is_err());
        assert!(validate_protocol_amounts(30, 10, 20, u128::MAX, 1).is_err());
    }

    #[test]
    fn empty_fixture_requires_pristine_bridge_state() {
        let empty = fixture_account(0, U256::ZERO);
        validate_fixture_state(
            &[empty],
            CheckPhase::Deposit,
            FixtureState::Empty,
            U256::ZERO,
            0,
            0,
        )
        .unwrap();

        let funded = fixture_account(0, U256::from(1));
        assert!(
            validate_fixture_state(
                &[funded],
                CheckPhase::Deposit,
                FixtureState::Empty,
                U256::ZERO,
                0,
                0,
            )
            .is_err()
        );
        assert!(
            validate_fixture_state(
                &[fixture_account(0, U256::ZERO)],
                CheckPhase::Deposit,
                FixtureState::Empty,
                U256::from(1),
                1,
                0,
            )
            .is_err()
        );
    }

    #[test]
    fn funded_fixture_requires_confirmed_deposits_and_portal_backing() {
        let states = [
            fixture_account(0, U256::from(10)),
            fixture_account(1, U256::from(20)),
        ];
        validate_fixture_state(
            &states,
            CheckPhase::Withdrawal,
            FixtureState::Funded,
            U256::from(30),
            2,
            2,
        )
        .unwrap();

        assert!(
            validate_fixture_state(
                &states,
                CheckPhase::Activity,
                FixtureState::Funded,
                U256::from(30),
                2,
                1,
            )
            .is_err()
        );
        assert!(
            validate_fixture_state(
                &states,
                CheckPhase::Activity,
                FixtureState::Funded,
                U256::from(29),
                2,
                2,
            )
            .is_err()
        );
        assert!(
            validate_fixture_state(
                &states,
                CheckPhase::Deposit,
                FixtureState::Funded,
                U256::from(30),
                2,
                2,
            )
            .is_err()
        );
    }

    #[test]
    fn ready_fixture_requires_only_the_confirmed_sequencer_bootstrap() {
        let states = [
            fixture_account(16, U256::ZERO),
            fixture_account(17, U256::ZERO),
        ];
        validate_fixture_state(
            &states,
            CheckPhase::Roundtrip,
            FixtureState::Ready,
            U256::from(1),
            1,
            1,
        )
        .unwrap();

        assert!(
            validate_fixture_state(
                &[fixture_account(16, U256::from(1))],
                CheckPhase::Roundtrip,
                FixtureState::Ready,
                U256::from(1),
                1,
                1,
            )
            .is_err()
        );
        assert!(
            validate_fixture_state(
                &states,
                CheckPhase::Roundtrip,
                FixtureState::Ready,
                U256::from(1),
                1,
                0,
            )
            .is_err()
        );
    }

    #[test]
    fn expiring_fee_cap_reserves_txgen_uniqueness_bump() {
        assert_eq!(expiring_fee_caps(100, 90, 25).unwrap(), (125, 115));
        assert!(expiring_fee_caps(u128::MAX, 90, 1).is_err());
        assert!(expiring_fee_caps(100, u128::MAX, 1).is_err());
    }

    #[test]
    fn derives_expected_account_range_without_serializing_mnemonic() {
        let signers = derive_signers(TEST_MNEMONIC, 2, 4).unwrap();
        assert_eq!(signers.len(), 2);
        assert_ne!(signers[0].address(), signers[1].address());

        let report = PreflightReport {
            check_phase: CheckPhase::All,
            fixture_state: Some(FixtureState::Funded),
            l1_chain_id: 1,
            zone_chain_id: 2,
            l1_client_version: "tempo/test".into(),
            zone_client_version: "tempo-zone/test".into(),
            l1_genesis_hash: alloy::primitives::B256::ZERO.to_string(),
            zone_genesis_hash: alloy::primitives::B256::ZERO.to_string(),
            zone_id: 3,
            portal: Address::ZERO.to_string(),
            outbox: Address::ZERO.to_string(),
            token: Address::ZERO.to_string(),
            deposit_fee: 1,
            bounceback_fee: 1,
            withdrawal_fee: 1,
            portal_token_balance: "1".into(),
            deposit_count: 1,
            last_processed_deposit_number: 1,
            queried_l1_gas_price: 1,
            queried_zone_gas_price: 1,
            l1_max_fee_per_gas: 1,
            zone_max_fee_per_gas: 1,
            approval_fee_bump: 1,
            activity_fee_bump: 1,
            activity_max_fee_per_gas: 2,
            transactions_per_account: 1,
            sponsored_approval_rounds: 1,
            bootstrap_deposit_amount: 10,
            bootstrap_minimum_deposit_amount: "10".into(),
            sponsored_approval_fee_required: "1".into(),
            portal_approval_setup_accounts: vec![],
            outbox_approval_setup_accounts: vec![],
            control_account: AccountReport::from(&fixture_account(0, U256::ZERO)),
            sequencer_account: AccountReport::from(&fixture_account(4, U256::ZERO)),
            accounts: vec![],
        };
        assert!(
            !serde_json::to_string(&report)
                .unwrap()
                .contains(TEST_MNEMONIC)
        );
    }

    #[test]
    fn renders_all_specs_and_expands_per_account_approvals() {
        let output = temp_output("render");
        let config = local_render_config();
        render_all_specs(&output, &config, true, &[0, 1], &[1]).unwrap();

        for name in [
            "deposit.yml",
            "zone-activity.yml",
            "withdrawal.yml",
            "bootstrap-deposit.yml",
            "zone-roundtrip.yml",
            "scenario-fragments.yml",
            "bootstrap-scenario.yml",
            "roundtrip-scenario.yml",
        ] {
            let contents = fs::read_to_string(output.join(name)).unwrap();
            let _: Value = serde_yaml::from_str(&contents).unwrap();
            assert!(!contents.contains("__"), "unresolved placeholder in {name}");
        }
        for name in [
            "deposit.yml",
            "zone-activity.yml",
            "withdrawal.yml",
            "zone-roundtrip.yml",
        ] {
            let contents = fs::read_to_string(output.join(name)).unwrap();
            let spec: Value = serde_yaml::from_str(&contents).unwrap();
            assert_eq!(spec["accounts"]["users"]["range"][0], 7);
            assert_eq!(spec["accounts"]["users"]["range"][1], 9);
            assert!(contents.contains("${ZONES_BENCH_MNEMONIC}"));
        }
        let deposit: Value =
            serde_yaml::from_str(&fs::read_to_string(output.join("deposit.yml")).unwrap()).unwrap();
        assert_eq!(deposit["setup"]["steps"].as_sequence().unwrap().len(), 2);
        assert!(
            deposit["setup"]["steps"][0]["tx"]
                .get("expiring_nonce")
                .is_none()
        );
        assert_eq!(
            deposit["templates"]["deposit"]["call"]["function"],
            "deposit(address,address,uint128,bytes32,address)"
        );
        let activity: Value =
            serde_yaml::from_str(&fs::read_to_string(output.join("zone-activity.yml")).unwrap())
                .unwrap();
        assert_eq!(
            activity["templates"]["tip20_transfer"]["expiring_nonce"],
            true
        );
        assert_eq!(
            activity["templates"]["tip20_transfer"]["valid_for_secs"],
            25
        );
        let withdrawal: Value =
            serde_yaml::from_str(&fs::read_to_string(output.join("withdrawal.yml")).unwrap())
                .unwrap();
        assert_eq!(withdrawal["setup"]["steps"].as_sequence().unwrap().len(), 1);
        assert!(
            withdrawal["setup"]["steps"][0]["tx"]
                .get("sponsor")
                .is_none()
        );
        assert!(
            withdrawal["setup"]["steps"][0]["tx"]
                .get("expiring_nonce")
                .is_none()
        );
        assert_eq!(
            withdrawal["templates"]["request_withdrawal"]["call"]["function"],
            "requestWithdrawal(address,address,uint128,bytes32,uint64,address,bytes,bytes)"
        );

        let bootstrap: Value = serde_yaml::from_str(
            &fs::read_to_string(output.join("bootstrap-deposit.yml")).unwrap(),
        )
        .unwrap();
        assert_eq!(bootstrap["accounts"]["control"]["range"][0], 0);
        assert_eq!(bootstrap["accounts"]["control"]["range"][1], 1);
        assert_eq!(bootstrap["setup"]["steps"].as_sequence().unwrap().len(), 1);
        assert!(
            bootstrap["setup"]["steps"][0]["tx"]
                .get("expiring_nonce")
                .is_none()
        );
        assert_eq!(
            bootstrap["templates"]["bootstrap_deposit"]["call"]["args"][1],
            config.sequencer.to_string()
        );

        let zone_roundtrip: Value =
            serde_yaml::from_str(&fs::read_to_string(output.join("zone-roundtrip.yml")).unwrap())
                .unwrap();
        let sponsored_approval = &zone_roundtrip["setup"]["steps"][0]["tx"];
        assert_eq!(sponsored_approval["sponsor"]["pool"], "sponsor");
        assert!(sponsored_approval.get("expiring_nonce").is_none());
        assert_eq!(sponsored_approval["call"]["function"], "approve");
        assert_eq!(
            zone_roundtrip["templates"]["request_withdrawal"]["call"]["function"],
            "requestWithdrawal(address,address,uint128,bytes32,uint64,address,bytes,bytes)"
        );

        let bootstrap_scenario: Value = serde_yaml::from_str(
            &fs::read_to_string(output.join("bootstrap-scenario.yml")).unwrap(),
        )
        .unwrap();
        assert_eq!(bootstrap_scenario["include"][0], "./scenario-fragments.yml");
        let bootstrap_steps = bootstrap_scenario["scenario"]["steps"]
            .as_sequence()
            .unwrap();
        assert_eq!(bootstrap_steps.len(), 3);
        assert_eq!(bootstrap_steps[2]["use"], "deposit-and-wait-zone");
        assert_eq!(bootstrap_steps[2]["as"], "bootstrap_deposit");
        assert_eq!(
            bootstrap_steps[2]["with"]["recipient"],
            config.sequencer.to_string()
        );
        assert_eq!(
            bootstrap_steps[2]["with"]["sender_address"]["var"],
            "account.address"
        );

        let scenario_fragments: Value = serde_yaml::from_str(
            &fs::read_to_string(output.join("scenario-fragments.yml")).unwrap(),
        )
        .unwrap();
        assert!(scenario_fragments.get("chains").is_none());
        assert!(scenario_fragments.get("scenario").is_none());
        let deposit_fragment = &scenario_fragments["fragments"]["deposit-and-wait-zone"];
        assert_eq!(deposit_fragment["parameters"]["sender"], "account_ref");
        assert_eq!(deposit_fragment["outputs"]["deposit_made"], "log");
        let fragment_steps = deposit_fragment["steps"].as_sequence().unwrap();
        assert_eq!(fragment_steps.len(), 3);
        assert_eq!(fragment_steps[0]["submit"]["template"]["param"], "template");
        assert_eq!(
            fragment_steps[0]["submit"]["with"]["call"]["args"][0],
            config.token.to_string()
        );
        assert_eq!(fragment_steps[1]["wait_log"]["event"], "DepositMade");
        assert_eq!(fragment_steps[2]["wait_log"]["event"], "DepositProcessed");

        let roundtrip_scenario: Value = serde_yaml::from_str(
            &fs::read_to_string(output.join("roundtrip-scenario.yml")).unwrap(),
        )
        .unwrap();
        assert_eq!(roundtrip_scenario["include"][0], "./scenario-fragments.yml");
        assert_eq!(
            roundtrip_scenario["chains"]["zone"]["rpc_url"],
            "${ZONE_PRIVATE_RPC_URL}"
        );
        assert_eq!(
            roundtrip_scenario["chains"]["zone"]["request_auth"]["sender_header"]["name"],
            "X-Authorization-Token"
        );
        let roundtrip_steps = roundtrip_scenario["scenario"]["steps"]
            .as_sequence()
            .unwrap();
        assert_eq!(roundtrip_steps.len(), 9);
        assert_eq!(roundtrip_steps[1]["use"], "deposit-and-wait-zone");
        assert_eq!(roundtrip_steps[1]["as"], "deposit_to_zone");
        assert_eq!(
            roundtrip_steps[7]["wait_log"]["event"],
            "WithdrawalRequested"
        );
        assert_eq!(
            roundtrip_steps[7]["wait_log"]["from_block"]["var"],
            "zone_before_withdrawal.block_number"
        );
        assert_eq!(
            roundtrip_steps[8]["wait_log"]["event"],
            "WithdrawalProcessed"
        );
        assert_eq!(
            roundtrip_steps[8]["wait_log"]["where"]["senderTag"]["keccak256_packed"]["types"][0],
            "address"
        );

        fs::remove_dir_all(output).unwrap();
    }

    #[test]
    fn renders_neobank_private_flow_assets_with_exact_terminal_boundaries() {
        let output = temp_output("neobank-render");
        let config = local_render_config();
        let mut replacements = common_replacements(&config);
        replacements.extend(HashMap::from([
            (
                "__ZONE_TOKEN__".into(),
                Value::from("0x2000000000000000000000000000000000000001"),
            ),
            (
                "__DLUSD__".into(),
                Value::from("0x2000000000000000000000000000000000000001"),
            ),
            (
                "__PATHUSD__".into(),
                Value::from("0x2000000000000000000000000000000000000002"),
            ),
            (
                "__EARN_TOKEN__".into(),
                Value::from("0x2000000000000000000000000000000000000003"),
            ),
            (
                "__GATEWAY__".into(),
                Value::from("0x3000000000000000000000000000000000000001"),
            ),
            (
                "__BRIDGE_WALLET__".into(),
                Value::from("0x3000000000000000000000000000000000000002"),
            ),
            (
                "__REWARDS__".into(),
                Value::from("0x3000000000000000000000000000000000000003"),
            ),
            ("__PRIVATE_TRANSFER_AMOUNT__".into(), Value::from(1_u64)),
            ("__EARN_DEPOSIT_AMOUNT__".into(), Value::from(100_u64)),
            ("__EARN_REDEEM_AMOUNT__".into(), Value::from(100_u64)),
            ("__OFFRAMP_AMOUNT__".into(), Value::from(1_u64)),
            ("__CALLBACK_GAS_LIMIT__".into(), Value::from(2_000_000_u64)),
            ("__ONRAMP_AMOUNT__".into(), Value::from(1_000_u64)),
            ("__WITHDRAWAL_ONLY_AMOUNT__".into(), Value::from(75_u64)),
            ("__WITHDRAWAL_SETUP_AMOUNT__".into(), Value::from(5_000_u64)),
            (
                "__REWARD_ONRAMP_PER_ACCOUNT__".into(),
                Value::from(2_000_u64),
            ),
            (
                "__REWARD_POSITION_PER_ACCOUNT__".into(),
                Value::from(1_000_u64),
            ),
            ("__REWARD_FUND_AMOUNT__".into(), Value::from(10_000_u64)),
            (
                "__REWARD_FUND_GAS_LIMIT__".into(),
                Value::from(5_000_000_u64),
            ),
            ("__REWARD_FIRST_REDEEM_AMOUNT__".into(), Value::from(40_u64)),
            (
                "__REWARD_SECOND_REDEEM_AMOUNT__".into(),
                Value::from(60_u64),
            ),
            ("__ZONE_ID__".into(), Value::from(1_u64)),
        ]));
        for source in [
            "../neobank/l1-onramp.yml",
            "../neobank/zone-flow.yml",
            "../neobank/scenario-fragments.yml",
            "../neobank/encrypted-deposit-scenario.yml",
            "../neobank/private-withdrawal-funding-scenario.yml",
            "../neobank/private-withdrawal-scenario.yml",
            "../neobank/private-flow-scenario.yml",
            "../neobank/swapped-lifecycle-scenario.yml",
            "../neobank/direct-lifecycle-scenario.yml",
            "../neobank/third-party-recipient-scenario.yml",
            "../neobank/slippage-bounce-scenario.yml",
            "../neobank/rewards-position-scenario.yml",
            "../neobank/rewards-funding-scenario.yml",
            "../neobank/rewards-redemption-scenario.yml",
        ] {
            let destination = output.join(Path::new(source).file_name().unwrap());
            render_document(source, &destination, &replacements, false).unwrap();
            let contents = fs::read_to_string(destination).unwrap();
            let _: Value = serde_yaml::from_str(&contents).unwrap();
            assert!(
                !contents.contains("__"),
                "unresolved placeholder in {source}"
            );
        }

        let zone: Value =
            serde_yaml::from_str(&fs::read_to_string(output.join("zone-flow.yml")).unwrap())
                .unwrap();
        assert_eq!(
            zone["templates"]["private_transfer"]["expiring_nonce"],
            true
        );
        for template in ["gateway_deposit", "gateway_redeem", "offramp"] {
            assert!(
                zone["templates"][template].get("expiring_nonce").is_none(),
                "{template} must use a regular nonce so it cannot expire under load"
            );
            assert!(
                zone["templates"][template].get("valid_for_secs").is_none(),
                "{template} must not have a transaction validity deadline"
            );
        }
        assert_eq!(
            zone["templates"]["gateway_deposit"]["call"]["function"],
            "requestWithdrawal(address,address,uint128,bytes32,uint64,address,bytes,bytes)"
        );
        assert_eq!(
            zone["templates"]["gateway_deposit"]["call"]["args"][4],
            2_000_000
        );
        assert_eq!(
            zone["templates"]["gateway_redeem"]["fee_token"],
            "0x2000000000000000000000000000000000000001"
        );
        assert_eq!(zone["templates"]["gateway_redeem"]["call"]["args"][7], "0x");
        assert_eq!(zone["templates"]["offramp"]["call"]["args"][4], 0);

        let encrypted_deposit: Value = serde_yaml::from_str(
            &fs::read_to_string(output.join("encrypted-deposit-scenario.yml")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            encrypted_deposit["scenario"]["name"],
            "neobank-encrypted-deposit"
        );
        assert_eq!(
            encrypted_deposit["scenario"]["bindings"]["account"]["account"]["select"],
            "lease"
        );
        let encrypted_deposit_steps = encrypted_deposit["scenario"]["steps"]
            .as_sequence()
            .unwrap();
        assert_eq!(encrypted_deposit_steps.len(), 1);
        let encrypted_deposit_step = &encrypted_deposit_steps[0];
        assert_eq!(encrypted_deposit_step["use"], "encrypted-zone-entry");
        assert_eq!(encrypted_deposit_step["as"], "onramp");
        assert_eq!(
            encrypted_deposit_step["with"]["sender"]["var"],
            "account.ref"
        );
        assert_eq!(
            encrypted_deposit_step["with"]["recipient"]["var"],
            "account.address"
        );
        assert_eq!(
            encrypted_deposit_step["with"]["token"],
            replacements["__DLUSD__"]
        );
        assert_eq!(
            encrypted_deposit_step["with"]["fee_token"],
            replacements["__DLUSD__"]
        );
        assert_eq!(encrypted_deposit_step["with"]["amount"], 1_000);
        assert_eq!(
            encrypted_deposit_step["with"]["memo"]["var"],
            "onramp_action_id"
        );

        let withdrawal_funding: Value = serde_yaml::from_str(
            &fs::read_to_string(output.join("private-withdrawal-funding-scenario.yml")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            withdrawal_funding["scenario"]["name"],
            "neobank-private-withdrawal-funding-setup"
        );
        assert_eq!(
            withdrawal_funding["scenario"]["bindings"]["account"]["account"]["select"],
            "lease"
        );
        let withdrawal_funding_steps = withdrawal_funding["scenario"]["steps"]
            .as_sequence()
            .unwrap();
        assert_eq!(withdrawal_funding_steps.len(), 1);
        let withdrawal_funding_step = &withdrawal_funding_steps[0];
        assert_eq!(withdrawal_funding_step["use"], "encrypted-zone-entry");
        assert_eq!(withdrawal_funding_step["as"], "funding");
        assert_eq!(
            withdrawal_funding_step["with"]["token"],
            replacements["__DLUSD__"]
        );
        assert_eq!(
            withdrawal_funding_step["with"]["fee_token"],
            replacements["__DLUSD__"]
        );
        assert_eq!(withdrawal_funding_step["with"]["amount"], 5_000);
        assert_eq!(
            withdrawal_funding_step["with"]["memo"]["var"],
            "funding_action_id"
        );

        let private_withdrawal: Value = serde_yaml::from_str(
            &fs::read_to_string(output.join("private-withdrawal-scenario.yml")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            private_withdrawal["scenario"]["name"],
            "neobank-private-withdrawal"
        );
        assert_eq!(
            private_withdrawal["scenario"]["bindings"]["account"]["account"]["select"],
            "lease"
        );
        let private_withdrawal_steps = private_withdrawal["scenario"]["steps"]
            .as_sequence()
            .unwrap();
        assert_eq!(private_withdrawal_steps.len(), 5);
        assert_eq!(private_withdrawal_steps[0]["checkpoint"]["chain"], "l1");

        let withdrawal_submit = &private_withdrawal_steps[1]["submit"];
        assert_eq!(withdrawal_submit["chain"], "zone");
        assert_eq!(withdrawal_submit["template"], "offramp");
        assert_eq!(withdrawal_submit["with"]["from"]["var"], "account.ref");
        assert_eq!(
            withdrawal_submit["with"]["fee_token"],
            replacements["__DLUSD__"]
        );
        let withdrawal_args = withdrawal_submit["with"]["call"]["args"]
            .as_sequence()
            .unwrap();
        assert_eq!(withdrawal_args.len(), 8);
        assert_eq!(withdrawal_args[0], replacements["__DLUSD__"]);
        assert_eq!(withdrawal_args[1], replacements["__BRIDGE_WALLET__"]);
        assert_eq!(withdrawal_args[2], 75);
        assert_eq!(withdrawal_args[3]["var"], "withdrawal_action_id");
        assert_eq!(withdrawal_args[4], 0);
        assert_eq!(withdrawal_args[5]["var"], "account.address");
        assert_eq!(withdrawal_args[6], "0x");
        assert_eq!(withdrawal_args[7], "0x");

        assert_eq!(
            private_withdrawal_steps[2]["wait_receipt"]["transaction_hash"]["var"],
            "withdrawal.tx_hash"
        );
        assert_eq!(private_withdrawal_steps[2]["timeout"], "45s");

        let withdrawal_requested = &private_withdrawal_steps[3]["wait_log"];
        assert_eq!(withdrawal_requested["event"], "WithdrawalRequested");
        assert_eq!(
            withdrawal_requested["from_block"]["var"],
            "withdrawal_receipt.block_number"
        );
        assert_eq!(
            withdrawal_requested["transaction_hash"]["var"],
            "withdrawal.tx_hash"
        );
        assert_eq!(withdrawal_requested["where"]["fee"], 0);
        assert_eq!(withdrawal_requested["where"]["gasLimit"], 0);
        assert_eq!(withdrawal_requested["where"]["data"], "0x");
        assert_eq!(withdrawal_requested["where"]["revealTo"], "0x");

        let withdrawal_processed = &private_withdrawal_steps[4]["wait_log"];
        assert_eq!(withdrawal_processed["event"], "WithdrawalProcessed");
        assert_eq!(
            withdrawal_processed["from_block"]["var"],
            "l1_before_withdrawal.block_number"
        );
        assert_eq!(
            withdrawal_processed["where"]["to"],
            replacements["__BRIDGE_WALLET__"]
        );
        assert_eq!(
            withdrawal_processed["where"]["token"],
            replacements["__DLUSD__"]
        );
        assert_eq!(withdrawal_processed["where"]["amount"], 75);
        assert_eq!(withdrawal_processed["where"]["callbackSuccess"], true);
        let sender_tag = &withdrawal_processed["where"]["senderTag"]["keccak256_packed"];
        assert_eq!(sender_tag["types"][0], "address");
        assert_eq!(sender_tag["types"][1], "bytes32");
        assert_eq!(sender_tag["values"][0]["var"], "account.address");
        assert_eq!(sender_tag["values"][1]["var"], "withdrawal.tx_hash");

        let scenario: Value = serde_yaml::from_str(
            &fs::read_to_string(output.join("private-flow-scenario.yml")).unwrap(),
        )
        .unwrap();
        let steps = scenario["scenario"]["steps"].as_sequence().unwrap();
        assert_eq!(steps.len(), 10);
        assert_eq!(steps[0]["use"], "encrypted-zone-entry");
        assert_eq!(steps[0]["as"], "onramp");
        assert_eq!(steps[0]["with"]["fee_token"], replacements["__DLUSD__"]);
        assert_eq!(
            steps[2]["wait_receipt"]["transaction_hash"]["var"],
            "private_transfer.tx_hash"
        );
        assert_eq!(steps[2]["timeout"], "45s");
        assert_eq!(steps[3]["use"], "earn-deposit-and-return");
        assert_eq!(steps[3]["as"], "earn_deposit");
        assert_eq!(steps[3]["with"]["fee_token"], replacements["__DLUSD__"]);
        assert_eq!(steps[4]["use"], "earn-redeem-and-return");
        assert_eq!(steps[4]["as"], "earn_redeem");
        assert_eq!(steps[4]["with"]["fee_token"], replacements["__DLUSD__"]);
        assert_eq!(
            steps[4]["with"]["amount"]["var"],
            "earn_deposit.callback.args.shares"
        );
        assert_eq!(
            steps[7]["wait_receipt"]["transaction_hash"]["var"],
            "offramp.tx_hash"
        );
        assert_eq!(steps[7]["timeout"], "45s");
        assert_eq!(steps[8]["wait_log"]["event"], "WithdrawalRequested");
        assert_eq!(
            steps[8]["wait_log"]["from_block"]["var"],
            "offramp_receipt.block_number"
        );
        assert_eq!(
            steps[8]["wait_log"]["transaction_hash"]["var"],
            "offramp.tx_hash"
        );
        assert_eq!(steps[8]["wait_log"]["where"]["gasLimit"], 0);
        assert_eq!(steps[8]["wait_log"]["where"]["fee"], 0);
        assert_eq!(steps[8]["wait_log"]["where"]["data"], "0x");
        assert_eq!(steps[8]["wait_log"]["where"]["revealTo"], "0x");
        assert_eq!(steps[9]["wait_log"]["event"], "WithdrawalProcessed");

        let swapped: Value = serde_yaml::from_str(
            &fs::read_to_string(output.join("swapped-lifecycle-scenario.yml")).unwrap(),
        )
        .unwrap();
        let redeem = &swapped["scenario"]["steps"][2];
        assert_eq!(
            redeem["with"]["fee_token"],
            "0x2000000000000000000000000000000000000001"
        );

        let direct: Value = serde_yaml::from_str(
            &fs::read_to_string(output.join("direct-lifecycle-scenario.yml")).unwrap(),
        )
        .unwrap();
        let direct_steps = direct["scenario"]["steps"].as_sequence().unwrap();
        assert_eq!(direct_steps.len(), 3);
        assert_eq!(direct_steps[0]["use"], "encrypted-zone-entry");
        assert_eq!(direct_steps[1]["use"], "earn-deposit-and-return");
        assert_eq!(direct_steps[2]["use"], "earn-redeem-and-return");
        for step in direct_steps {
            assert_eq!(
                step["with"]["fee_token"],
                "0x2000000000000000000000000000000000000002"
            );
        }
        assert_eq!(
            direct_steps[0]["with"]["token"],
            "0x2000000000000000000000000000000000000002"
        );
        assert_eq!(
            direct_steps[1]["with"]["input_token"],
            "0x2000000000000000000000000000000000000002"
        );
        assert_eq!(
            direct_steps[2]["with"]["output_token"],
            "0x2000000000000000000000000000000000000002"
        );
        assert_eq!(
            direct_steps[2]["with"]["amount"]["var"],
            "earn_deposit.callback.args.shares"
        );

        let third_party: Value = serde_yaml::from_str(
            &fs::read_to_string(output.join("third-party-recipient-scenario.yml")).unwrap(),
        )
        .unwrap();
        let third_party_bindings = &third_party["scenario"]["bindings"];
        for account in ["account_a", "account_b"] {
            assert_eq!(third_party_bindings[account]["account"]["pool"], "users");
            assert_eq!(third_party_bindings[account]["account"]["select"], "lease");
        }
        assert_eq!(
            third_party_bindings
                .as_mapping()
                .unwrap()
                .values()
                .filter(|binding| binding["account"]["select"] == "lease")
                .count(),
            2
        );

        let third_party_steps = third_party["scenario"]["steps"].as_sequence().unwrap();
        assert_eq!(third_party_steps.len(), 4);
        for step in third_party_steps {
            assert_eq!(
                step["with"]["fee_token"],
                "0x2000000000000000000000000000000000000002"
            );
        }
        assert_eq!(third_party_steps[0]["as"], "account_a_entry");
        assert_eq!(third_party_steps[0]["use"], "encrypted-zone-entry");
        assert_eq!(
            third_party_steps[0]["with"]["sender"]["var"],
            "account_a.ref"
        );
        assert_eq!(
            third_party_steps[0]["with"]["recipient"]["var"],
            "account_a.address"
        );
        assert_eq!(
            third_party_steps[0]["with"]["token"],
            "0x2000000000000000000000000000000000000002"
        );
        assert_eq!(third_party_steps[1]["as"], "account_b_fee_entry");
        assert_eq!(third_party_steps[1]["use"], "encrypted-zone-entry");
        assert_eq!(
            third_party_steps[1]["with"]["sender"]["var"],
            "account_b.ref"
        );
        assert_eq!(
            third_party_steps[1]["with"]["recipient"]["var"],
            "account_b.address"
        );

        let third_party_deposit = &third_party_steps[2];
        assert_eq!(third_party_deposit["as"], "earn_deposit");
        assert_eq!(third_party_deposit["use"], "earn-deposit-and-return");
        assert_eq!(
            third_party_deposit["with"]["sender"]["var"],
            "account_a.ref"
        );
        assert_eq!(
            third_party_deposit["with"]["recipient"]["var"],
            "account_b.address"
        );
        assert_eq!(
            third_party_deposit["with"]["input_token"],
            "0x2000000000000000000000000000000000000002"
        );
        for field in ["fallback_recipient", "refund_recipient"] {
            assert_eq!(
                third_party_deposit["with"][field]["var"],
                "account_a.address"
            );
        }

        let third_party_redeem = &third_party_steps[3];
        assert_eq!(third_party_redeem["as"], "earn_redeem");
        assert_eq!(third_party_redeem["use"], "earn-redeem-and-return");
        assert_eq!(third_party_redeem["with"]["sender"]["var"], "account_b.ref");
        assert_eq!(
            third_party_redeem["with"]["recipient"]["var"],
            "account_a.address"
        );
        assert_eq!(
            third_party_redeem["with"]["output_token"],
            "0x2000000000000000000000000000000000000002"
        );
        assert_eq!(
            third_party_redeem["with"]["amount"]["var"],
            "earn_deposit.callback.args.shares"
        );
        for field in ["fallback_recipient", "refund_recipient"] {
            assert_eq!(
                third_party_redeem["with"][field]["var"],
                "account_b.address"
            );
        }

        let bounce: Value = serde_yaml::from_str(
            &fs::read_to_string(output.join("slippage-bounce-scenario.yml")).unwrap(),
        )
        .unwrap();
        let bounce_steps = bounce["scenario"]["steps"].as_sequence().unwrap();
        assert_eq!(bounce_steps.len(), 2);
        assert_eq!(bounce_steps[0]["use"], "encrypted-zone-entry");
        assert_eq!(bounce_steps[1]["use"], "earn-deposit-expect-bounce");
        assert_eq!(
            bounce_steps[1]["with"]["fallback_recipient"]["var"],
            "account.address"
        );

        let reward_position: Value = serde_yaml::from_str(
            &fs::read_to_string(output.join("rewards-position-scenario.yml")).unwrap(),
        )
        .unwrap();
        let reward_position_steps = reward_position["scenario"]["steps"].as_sequence().unwrap();
        assert_eq!(reward_position_steps.len(), 2);
        assert_eq!(reward_position_steps[0]["use"], "encrypted-zone-entry");
        assert_eq!(
            reward_position_steps[0]["with"]["token"],
            "0x2000000000000000000000000000000000000002"
        );
        assert_eq!(reward_position_steps[0]["with"]["amount"], 2_000);
        assert_eq!(reward_position_steps[1]["use"], "earn-deposit-and-return");
        assert_eq!(reward_position_steps[1]["with"]["amount"], 1_000);

        let reward_funding: Value = serde_yaml::from_str(
            &fs::read_to_string(output.join("rewards-funding-scenario.yml")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            reward_funding["scenario"]["bindings"]["control"]["account"]["select"],
            "lease"
        );
        let reward_funding_steps = reward_funding["scenario"]["steps"].as_sequence().unwrap();
        assert_eq!(reward_funding_steps.len(), 3);
        assert_eq!(reward_funding_steps[0]["submit"]["await"], "receipt");
        assert_eq!(reward_funding_steps[1]["submit"]["await"], "receipt");
        assert_eq!(reward_funding_steps[2]["wait_log"]["event"], "Funded");
        for field in ["requested", "funded"] {
            assert_eq!(reward_funding_steps[2]["wait_log"]["where"][field], 10_000);
        }

        let reward_redemption: Value = serde_yaml::from_str(
            &fs::read_to_string(output.join("rewards-redemption-scenario.yml")).unwrap(),
        )
        .unwrap();
        let reward_redemption_steps = reward_redemption["scenario"]["steps"]
            .as_sequence()
            .unwrap();
        assert_eq!(reward_redemption_steps.len(), 2);
        for (step, amount) in reward_redemption_steps.iter().zip([40, 60]) {
            assert_eq!(step["use"], "earn-redeem-and-return");
            assert_eq!(step["with"]["amount"], amount);
            assert_eq!(
                step["with"]["output_token"],
                "0x2000000000000000000000000000000000000002"
            );
        }
        let reward_action_domains = [
            &reward_position_steps[0]["with"]["memo"]["keccak256_packed"]["values"][0],
            &reward_position_steps[1]["with"]["action_id"]["keccak256_packed"]["values"][0],
            &reward_redemption_steps[0]["with"]["action_id"]["keccak256_packed"]["values"][0],
            &reward_redemption_steps[1]["with"]["action_id"]["keccak256_packed"]["values"][0],
        ];
        assert!(reward_action_domains.iter().all(|domain| {
            domain
                .as_str()
                .is_some_and(|value| value.len() == 66 && value.starts_with("0x"))
        }));
        assert_eq!(
            reward_action_domains
                .iter()
                .filter_map(|domain| domain.as_str())
                .collect::<std::collections::HashSet<_>>()
                .len(),
            reward_action_domains.len(),
            "reward setup and measured action IDs must use disjoint domains"
        );

        let fragments: Value = serde_yaml::from_str(
            &fs::read_to_string(output.join("scenario-fragments.yml")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            fragments["fragments"]["wait-encrypted-zone-deposit"]["steps"][0]["wait_log"]["where"]
                ["amount"]["param"],
            "amount"
        );
        assert_eq!(
            fragments["fragments"]["encrypted-zone-entry"]["steps"][4]["with"]["amount"]["var"],
            "enqueued.args.netAmount"
        );
        assert_eq!(
            fragments["fragments"]["earn-deposit-and-return"]["steps"][8]["with"]["amount"]["var"],
            "callback.args.shares"
        );
        assert_eq!(
            fragments["fragments"]["earn-redeem-and-return"]["steps"][8]["with"]["amount"]["var"],
            "callback.args.outputAmount"
        );
        for name in ["earn-deposit-and-return", "earn-redeem-and-return"] {
            let receipt = &fragments["fragments"][name]["steps"][4];
            assert_eq!(
                receipt["wait_receipt"]["transaction_hash"]["var"],
                "request.tx_hash"
            );
            assert_eq!(receipt["timeout"], "45s");
            let requested = &fragments["fragments"][name]["steps"][5];
            assert_eq!(
                requested["wait_log"]["from_block"]["var"],
                "zone_before.block_number"
            );
            assert!(requested["wait_log"]["transaction_hash"].is_mapping());
            assert_eq!(requested["timeout"], "45s");
        }
        let bounce_fragment = &fragments["fragments"]["earn-deposit-expect-bounce"];
        let bounce_callback = &bounce_fragment["steps"][3]["submit"]["with"]["call"]["args"][6]["abi_encode"]
            ["values"][0];
        assert_eq!(bounce_callback["flow"], 0);
        assert_eq!(
            bounce_callback["minVaultAssets"],
            "340282366920938463463374607431768211455"
        );
        assert_eq!(bounce_callback["minVaultShares"]["param"], "amount");
        assert_eq!(
            bounce_fragment["steps"][6]["wait_log"]["where"]["callbackSuccess"],
            false
        );
        assert_eq!(
            bounce_fragment["steps"][7]["wait_log"]["transaction_hash"]["var"],
            "withdrawal_processed.transaction_hash"
        );
        assert_eq!(
            bounce_fragment["steps"][7]["wait_log"]["where"]["fallbackNonce"]["var"],
            "requested.args.fallbackNonce"
        );
        assert_eq!(
            bounce_fragment["steps"][8]["wait_log"]["event"],
            "WithdrawalBounceBackProcessed"
        );
        for (fragment, flow) in [
            ("earn-deposit-and-return", 0_u64),
            ("earn-redeem-and-return", 1_u64),
        ] {
            let encoded = &fragments["fragments"][fragment]["steps"][3]["submit"]["with"]["call"]["args"]
                [6]["abi_encode"];
            assert!(
                encoded["types"][0]
                    .as_str()
                    .is_some_and(|value| value.starts_with("tuple(uint8 flow,")),
                "missing dynamically encoded callback for {fragment}"
            );
            assert_eq!(encoded["values"][0]["flow"], flow);
            assert_eq!(encoded["values"][0]["actionId"]["param"], "action_id");
            assert_eq!(
                encoded["values"][0]["encrypted"]["var"],
                "encryption.encrypted"
            );
        }
        assert_eq!(
            fragments["fragments"]["earn-deposit-and-return"]["steps"][3]["submit"]["with"]["call"]
                ["args"][6]["abi_encode"]["values"][0]["outputToken"],
            "0x2000000000000000000000000000000000000003"
        );
        assert_eq!(
            fragments["fragments"]["earn-redeem-and-return"]["steps"][3]["submit"]["with"]["call"]
                ["args"][6]["abi_encode"]["values"][0]["outputToken"]["param"],
            "output_token"
        );
        for fragment in [
            "encrypted-zone-entry",
            "earn-deposit-and-return",
            "earn-redeem-and-return",
        ] {
            assert!(
                fragments["fragments"][fragment]["steps"]
                    .as_sequence()
                    .unwrap()
                    .iter()
                    .any(|step| step["invoke"]["action"] == "prepare_encrypted_deposit"),
                "{fragment} must prepare its encrypted payload in memory"
            );
        }
        assert!(steps.iter().any(|step| {
            step["wait_log"]["event"] == "WithdrawalProcessed"
                && step["wait_log"]["where"]["senderTag"]["keccak256_packed"]["types"][0]
                    == "address"
        }));

        let swapped_steps = swapped["scenario"]["steps"].as_sequence().unwrap();
        assert_eq!(swapped_steps.len(), 3);
        assert_eq!(swapped_steps[0]["use"], "encrypted-zone-entry");
        assert_eq!(swapped_steps[1]["use"], "earn-deposit-and-return");
        assert_eq!(swapped_steps[2]["use"], "earn-redeem-and-return");
        fs::remove_dir_all(output).unwrap();
    }

    /// Compatibility smoke test for the separately installed transaction generator.
    ///
    /// Zones CI does not install txgen-tempo today, so this returns early when the binary is not
    /// present. Set TXGEN_TEMPO_BIN to exercise a pinned binary in CI or locally.
    #[test]
    fn txgen_generates_representative_local_transactions_when_installed() {
        let configured_txgen = std::env::var_os("TXGEN_TEMPO_BIN");
        let require_scenario_support = configured_txgen.is_some();
        let txgen = configured_txgen.unwrap_or_else(|| "txgen-tempo".into());
        match Command::new(&txgen).arg("--help").output() {
            Ok(output) if output.status.success() => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("skipping txgen compatibility smoke test: txgen-tempo is not installed");
                return;
            }
            Ok(output) => panic!(
                "txgen-tempo --help failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ),
            Err(error) => panic!("failed executing txgen-tempo: {error}"),
        }

        let output = temp_output("generate");
        let config = local_render_config();
        render_all_specs(&output, &config, true, &[0, 1], &[0, 1]).unwrap();
        let pool_addresses =
            derive_signers(TEST_MNEMONIC, config.account_start, config.account_end)
                .unwrap()
                .into_iter()
                .map(|signer| signer.address())
                .collect::<Vec<_>>();

        let deposit = generate(&txgen, &output.join("deposit.yml"));
        assert_setup_approvals(
            &deposit,
            config.token,
            config.portal,
            2,
            config.l1_max_fee_per_gas,
        );
        assert_workload_inclusion_keys(&deposit);
        let deposits = workload_envelopes(&deposit);
        assert_eq!(deposits.len(), 2);
        let deposit = &deposits[0];
        assert_eq!(deposit.chain_id(), Some(config.l1_chain_id));
        assert_eq!(deposit.fee_token(), Some(config.token));
        assert!(!deposit.is_expiring_nonce());
        let sender = deposit.recover_signer().unwrap();
        assert!(pool_addresses.contains(&sender));
        let (target, input) = only_call(deposit);
        assert_eq!(target, config.portal);
        let call = ZonePortal::depositCall::abi_decode(input).unwrap();
        assert_eq!(call.token, config.token);
        assert_eq!(call.to, sender);
        assert_eq!(call.amount, config.deposit_amount);
        assert_ne!(call.memo, alloy::primitives::B256::ZERO);
        assert_eq!(call.tempoRefundRecipient, sender);
        let (_, second_input) = only_call(&deposits[1]);
        let second_call = ZonePortal::depositCall::abi_decode(second_input).unwrap();
        assert_ne!(call.memo, second_call.memo);

        let activity = generate(&txgen, &output.join("zone-activity.yml"));
        assert_workload_inclusion_keys(&activity);
        let activities = workload_envelopes(&activity);
        assert_eq!(activities.len(), 2);
        let activity = &activities[0];
        assert_eq!(activity.chain_id(), Some(config.zone_chain_id));
        assert_eq!(activity.fee_token(), Some(config.token));
        assert!(activity.is_expiring_nonce());
        assert_eq!(activity.max_fee_per_gas(), config.zone_max_fee_per_gas + 1);
        assert_eq!(
            activities[1].max_fee_per_gas(),
            config.zone_max_fee_per_gas + 2
        );
        let (target, input) = only_call(activity);
        assert_eq!(target, config.token);
        let call = ITIP20::transferCall::abi_decode(input).unwrap();
        assert_eq!(call.amount, U256::from(config.activity_amount));
        assert!(pool_addresses.contains(&call.to));

        let withdrawal = generate(&txgen, &output.join("withdrawal.yml"));
        assert_setup_approvals(
            &withdrawal,
            config.token,
            config.outbox,
            2,
            config.zone_max_fee_per_gas,
        );
        assert_workload_inclusion_keys(&withdrawal);
        let withdrawals = workload_envelopes(&withdrawal);
        assert_eq!(withdrawals.len(), 2);
        let withdrawal = &withdrawals[0];
        assert_eq!(withdrawal.chain_id(), Some(config.zone_chain_id));
        assert_eq!(withdrawal.fee_token(), Some(config.token));
        let sender = withdrawal.recover_signer().unwrap();
        assert!(pool_addresses.contains(&sender));
        let (target, input) = only_call(withdrawal);
        assert_eq!(target, config.outbox);
        let call = IZoneOutbox::requestWithdrawalCall::abi_decode(input).unwrap();
        assert_eq!(call.token, config.token);
        assert_eq!(call.to, sender);
        assert_eq!(call.amount, config.withdrawal_amount);
        assert_ne!(call.memo, alloy::primitives::B256::ZERO);
        assert_eq!(call.gasLimit, 0);
        assert_eq!(call.zoneFallbackRecipient, sender);
        assert!(call.data.is_empty());
        assert!(call.revealTo.is_empty());
        let (_, second_input) = only_call(&withdrawals[1]);
        let second_call = IZoneOutbox::requestWithdrawalCall::abi_decode(second_input).unwrap();
        assert_ne!(call.memo, second_call.memo);

        let bootstrap = generate(&txgen, &output.join("bootstrap-deposit.yml"));
        assert_setup_approvals(
            &bootstrap,
            config.token,
            config.portal,
            1,
            config.l1_max_fee_per_gas,
        );
        assert_workload_inclusion_keys(&bootstrap);
        let bootstrap_transactions = workload_envelopes(&bootstrap);
        assert_eq!(bootstrap_transactions.len(), 2);
        let (target, input) = only_call(&bootstrap_transactions[0]);
        assert_eq!(target, config.portal);
        let call = ZonePortal::depositCall::abi_decode(input).unwrap();
        assert_eq!(call.to, config.sequencer);
        assert_eq!(call.amount, config.bootstrap_deposit_amount);
        assert_ne!(call.memo, alloy::primitives::B256::ZERO);

        let zone_roundtrip = generate(&txgen, &output.join("zone-roundtrip.yml"));
        assert_setup_approvals(
            &zone_roundtrip,
            config.token,
            config.outbox,
            2,
            config.zone_max_fee_per_gas,
        );
        let setup = zone_roundtrip
            .iter()
            .find(|tx| tx["phase"] == "setup")
            .expect("sponsored approval must be generated");
        let setup = decode_envelope(setup);
        let sender = setup.recover_signer().unwrap();
        assert_eq!(setup.fee_payer(sender).unwrap(), config.sequencer);
        assert_workload_inclusion_keys(&zone_roundtrip);

        let scenario_render = Command::new(&txgen)
            .args(["scenario", "render", "--help"])
            .output()
            .unwrap();
        if scenario_render.status.success() {
            fs::write(output.join("zone-auth.json"), "{}").unwrap();
            let bootstrap_scenario = render_scenario(
                &txgen,
                &output.join("bootstrap-scenario.yml"),
                &output.join("bootstrap-scenario.rendered.yml"),
                &output,
            );
            assert_flattened_scenario(&bootstrap_scenario, 5);
            assert_eq!(
                bootstrap_scenario["scenario"]["steps"][2]["save"],
                "bootstrap_deposit.submission"
            );

            let roundtrip_scenario = render_scenario(
                &txgen,
                &output.join("roundtrip-scenario.yml"),
                &output.join("roundtrip-scenario.rendered.yml"),
                &output,
            );
            assert_flattened_scenario(&roundtrip_scenario, 11);
            assert_eq!(
                roundtrip_scenario["scenario"]["steps"][1]["save"],
                "deposit_to_zone.submission"
            );

            validate_neobank_scenario(&txgen);
        } else if require_scenario_support {
            panic!(
                "configured txgen-tempo lacks scenario composition support: {}",
                String::from_utf8_lossy(&scenario_render.stderr)
            );
        } else {
            eprintln!("skipping scenario composition smoke test: installed txgen is too old");
        }

        fs::remove_dir_all(output).unwrap();
    }

    fn local_render_config() -> RenderConfig {
        RenderConfig {
            l1_chain_id: 42431,
            zone_chain_id: 42432,
            account_start: 7,
            account_end: 9,
            control_account_index: 0,
            sequencer_account_index: 4,
            sequencer: derive_signer(TEST_MNEMONIC, 4).unwrap().address(),
            portal: address!("0x0000000000000000000000000000000000001000"),
            outbox: ZONE_OUTBOX_ADDRESS,
            token: address!("0x20c0000000000000000000000000000000000000"),
            deposit_amount: 1_000_000,
            activity_amount: 1,
            withdrawal_amount: 100_000,
            bootstrap_deposit_amount: 10_000_000,
            l1_max_fee_per_gas: 100_000_000_000,
            l1_max_priority_fee_per_gas: 100_000_000_000,
            zone_max_fee_per_gas: 200_000_000_000,
            zone_max_priority_fee_per_gas: 200_000_000_000,
            deposit_gas_limit: 2_000_000,
            activity_gas_limit: 500_000,
            withdrawal_tx_gas_limit: 10_000_000,
            approval_gas_limit: 2_000_000,
        }
    }

    fn fixture_account(index: u32, zone_balance: U256) -> AccountState {
        AccountState {
            index,
            address: Address::with_last_byte(index as u8 + 1),
            l1_balance: U256::ZERO,
            portal_allowance: U256::ZERO,
            zone_balance,
            outbox_allowance: U256::ZERO,
        }
    }

    fn temp_output(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let output = std::env::temp_dir().join(format!(
            "zones-txgen-{label}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&output).unwrap();
        output
    }

    fn generate(txgen: &std::ffi::OsStr, spec: &Path) -> Vec<serde_json::Value> {
        let output = Command::new(txgen)
            .arg("generate")
            .arg("--spec")
            .arg(spec)
            .arg("--count")
            .arg("2")
            .arg("--seed")
            .arg("7")
            .env("ZONES_BENCH_MNEMONIC", TEST_MNEMONIC)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "txgen-tempo failed for {}\nstdout:\n{}\nstderr:\n{}",
            spec.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn render_scenario(
        txgen: &std::ffi::OsStr,
        scenario: &Path,
        rendered: &Path,
        output_dir: &Path,
    ) -> Value {
        let output = Command::new(txgen)
            .args(["scenario", "render", "--scenario"])
            .arg(scenario)
            .arg("--output")
            .arg(rendered)
            .env("ZONES_BENCH_MNEMONIC", TEST_MNEMONIC)
            .env("L1_RPC_URL", "http://127.0.0.1:18545")
            .env("ZONES_BENCH_L1_QUERY_RPC_URL", "http://127.0.0.1:18546")
            .env("ZONE_RPC_URL", "http://127.0.0.1:19545")
            .env("ZONE_PRIVATE_RPC_URL", "http://127.0.0.1:19546")
            .env(
                "ZONES_BENCH_ZONE_AUTH_MAP",
                output_dir.join("zone-auth.json"),
            )
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "txgen-tempo scenario render failed for {}\nstdout:\n{}\nstderr:\n{}",
            scenario.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_yaml::from_str(&fs::read_to_string(rendered).unwrap()).unwrap()
    }

    fn assert_flattened_scenario(scenario: &Value, expected_steps: usize) {
        assert!(scenario.get("include").is_none());
        assert!(scenario.get("fragments").is_none());
        let steps = scenario["scenario"]["steps"].as_sequence().unwrap();
        assert_eq!(steps.len(), expected_steps);
        assert!(steps.iter().all(|step| step.get("use").is_none()));
    }

    fn validate_neobank_scenario(txgen: &std::ffi::OsStr) {
        let output = temp_output("neobank-scenario-validation");
        let config = local_render_config();
        let mut replacements = common_replacements(&config);
        replacements.extend(HashMap::from([
            (
                "__ZONE_TOKEN__".into(),
                Value::from("0x2000000000000000000000000000000000000001"),
            ),
            (
                "__DLUSD__".into(),
                Value::from("0x2000000000000000000000000000000000000001"),
            ),
            (
                "__PATHUSD__".into(),
                Value::from("0x2000000000000000000000000000000000000002"),
            ),
            (
                "__EARN_TOKEN__".into(),
                Value::from("0x2000000000000000000000000000000000000003"),
            ),
            (
                "__GATEWAY__".into(),
                Value::from("0x3000000000000000000000000000000000000001"),
            ),
            (
                "__BRIDGE_WALLET__".into(),
                Value::from("0x3000000000000000000000000000000000000002"),
            ),
            (
                "__REWARDS__".into(),
                Value::from("0x3000000000000000000000000000000000000003"),
            ),
            ("__PRIVATE_TRANSFER_AMOUNT__".into(), Value::from(1_u64)),
            ("__EARN_DEPOSIT_AMOUNT__".into(), Value::from(100_u64)),
            ("__EARN_REDEEM_AMOUNT__".into(), Value::from(100_u64)),
            ("__OFFRAMP_AMOUNT__".into(), Value::from(1_u64)),
            ("__CALLBACK_GAS_LIMIT__".into(), Value::from(2_000_000_u64)),
            ("__ONRAMP_AMOUNT__".into(), Value::from(1_000_u64)),
            ("__WITHDRAWAL_ONLY_AMOUNT__".into(), Value::from(75_u64)),
            ("__WITHDRAWAL_SETUP_AMOUNT__".into(), Value::from(5_000_u64)),
            (
                "__REWARD_ONRAMP_PER_ACCOUNT__".into(),
                Value::from(2_000_u64),
            ),
            (
                "__REWARD_POSITION_PER_ACCOUNT__".into(),
                Value::from(1_000_u64),
            ),
            ("__REWARD_FUND_AMOUNT__".into(), Value::from(10_000_u64)),
            (
                "__REWARD_FUND_GAS_LIMIT__".into(),
                Value::from(5_000_000_u64),
            ),
            ("__REWARD_FIRST_REDEEM_AMOUNT__".into(), Value::from(40_u64)),
            (
                "__REWARD_SECOND_REDEEM_AMOUNT__".into(),
                Value::from(60_u64),
            ),
            ("__ZONE_ID__".into(), Value::from(1_u64)),
        ]));
        for source in [
            "../neobank/l1-onramp.yml",
            "../neobank/zone-flow.yml",
            "../neobank/scenario-fragments.yml",
            "../neobank/encrypted-deposit-scenario.yml",
            "../neobank/private-withdrawal-funding-scenario.yml",
            "../neobank/private-withdrawal-scenario.yml",
            "../neobank/private-flow-scenario.yml",
            "../neobank/swapped-lifecycle-scenario.yml",
            "../neobank/direct-lifecycle-scenario.yml",
            "../neobank/third-party-recipient-scenario.yml",
            "../neobank/slippage-bounce-scenario.yml",
            "../neobank/rewards-position-scenario.yml",
            "../neobank/rewards-funding-scenario.yml",
            "../neobank/rewards-redemption-scenario.yml",
        ] {
            let destination = output.join(Path::new(source).file_name().unwrap());
            render_document(source, &destination, &replacements, false).unwrap();
        }

        let txgen_abis = output.join("txgen/abis");
        fs::create_dir_all(&txgen_abis).unwrap();
        for name in ["tip20.json", "zone-outbox.json"] {
            fs::copy(
                Path::new(SOURCE_DIR).join("abis").join(name),
                txgen_abis.join(name),
            )
            .unwrap();
        }
        let fixture_abis = output.join("abis");
        fs::create_dir_all(&fixture_abis).unwrap();
        for name in [
            "vault-adapter.json",
            "vault-rewards.json",
            "zone-gateway.json",
            "zone-inbox.json",
            "zone-portal.json",
        ] {
            fs::copy(
                Path::new(SOURCE_DIR).join("../neobank/abis").join(name),
                fixture_abis.join(name),
            )
            .unwrap();
        }

        for (scenario, expected_steps) in [
            ("encrypted-deposit-scenario.yml", 5),
            ("private-withdrawal-funding-scenario.yml", 5),
            ("private-withdrawal-scenario.yml", 5),
            ("private-flow-scenario.yml", 30),
            ("swapped-lifecycle-scenario.yml", 23),
            ("direct-lifecycle-scenario.yml", 23),
            ("third-party-recipient-scenario.yml", 28),
            ("slippage-bounce-scenario.yml", 14),
            ("rewards-position-scenario.yml", 14),
            ("rewards-funding-scenario.yml", 3),
            ("rewards-redemption-scenario.yml", 18),
        ] {
            let rendered_path = output.join(format!("{scenario}.rendered.yml"));
            let validation = Command::new(txgen)
                .arg("scenario")
                .arg("render")
                .arg("--scenario")
                .arg(output.join(scenario))
                .arg("--output")
                .arg(&rendered_path)
                .env("ZONES_BENCH_MNEMONIC", TEST_MNEMONIC)
                .env("L1_RPC_URL", "http://l1.invalid")
                .env("ZONES_BENCH_L1_QUERY_RPC_URL", "http://l1-query.invalid")
                .env("ZONE_PRIVATE_RPC_URL", "http://zone-private.invalid")
                .env("ZONE_RPC_URL", "http://zone-query.invalid")
                .env("ZONES_BENCH_ZONE_AUTH_MAP", output.join("zone-auth.json"))
                .output()
                .unwrap();
            assert!(
                validation.status.success(),
                "txgen-tempo scenario render failed for {scenario}\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&validation.stdout),
                String::from_utf8_lossy(&validation.stderr)
            );
            let rendered: Value =
                serde_yaml::from_str(&fs::read_to_string(rendered_path).unwrap()).unwrap();
            assert_flattened_scenario(&rendered, expected_steps);
        }
        fs::remove_dir_all(output).unwrap();
    }

    fn workload_envelopes(generated: &[serde_json::Value]) -> Vec<TempoTxEnvelope> {
        generated
            .iter()
            .filter(|tx| tx["phase"] == "workload")
            .map(decode_envelope)
            .collect()
    }

    fn assert_workload_inclusion_keys(generated: &[serde_json::Value]) {
        for transaction in generated.iter().filter(|tx| tx["phase"] == "workload") {
            assert!(
                transaction["inclusion_keys"]
                    .as_array()
                    .is_some_and(|keys| !keys.is_empty()),
                "workload transaction must include receipt-tracking keys: {transaction}"
            );
        }
    }

    fn decode_envelope(tx: &serde_json::Value) -> TempoTxEnvelope {
        let raw = tx["raw"].as_str().expect("raw transaction must be hex");
        let raw = const_hex::decode(raw).unwrap();
        TempoTxEnvelope::decode_2718_exact(&raw).unwrap()
    }

    fn only_call(envelope: &TempoTxEnvelope) -> (Address, &[u8]) {
        let mut calls = envelope.calls();
        let (kind, input) = calls.next().expect("transaction must contain one call");
        assert!(calls.next().is_none());
        let TxKind::Call(target) = kind else {
            panic!("benchmark transaction must not create a contract")
        };
        (target, input)
    }

    fn assert_setup_approvals(
        generated: &[serde_json::Value],
        token: Address,
        spender: Address,
        expected: usize,
        base_max_fee_per_gas: u128,
    ) {
        let setup = generated
            .iter()
            .filter(|tx| tx["phase"] == "setup")
            .collect::<Vec<_>>();
        assert_eq!(setup.len(), expected);

        let mut submission_keys = std::collections::HashSet::new();
        let mut shared_inclusion_key = None;
        for (index, setup) in setup.into_iter().enumerate() {
            let tx_submission_keys = setup["submission_keys"].as_array().unwrap();
            assert_eq!(tx_submission_keys.len(), 1);
            assert!(submission_keys.insert(tx_submission_keys[0].as_str().unwrap()));

            let inclusion_keys = setup["inclusion_keys"].as_array().unwrap();
            assert_eq!(inclusion_keys.len(), 1);
            let inclusion_key = inclusion_keys[0].as_str().unwrap();
            assert_eq!(
                *shared_inclusion_key.get_or_insert(inclusion_key),
                inclusion_key
            );

            let envelope = decode_envelope(setup);
            assert!(
                !envelope.is_expiring_nonce(),
                "untimed setup approval {index} must not expire"
            );
            assert_eq!(envelope.max_fee_per_gas(), base_max_fee_per_gas);
            let (target, input) = only_call(&envelope);
            assert_eq!(target, token);
            let call = ITIP20::approveCall::abi_decode(input).unwrap();
            assert_eq!(call.spender, spender);
            assert_eq!(call.amount, U256::MAX);
        }
    }
}
