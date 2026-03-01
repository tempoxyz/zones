use alloy::genesis::Genesis;
use alloy_consensus::Header;
use alloy_primitives::{Address, B256, U256, keccak256};
use alloy_rlp::Encodable;
use alloy_rpc_types_eth::Filter;
use alloy_sol_types::{SolEvent, SolValue};
use eyre::WrapErr;
use reth_ethereum::tasks::TaskManager;
use reth_node_api::FullNodeComponents;
use reth_node_builder::{NodeBuilder, NodeConfig, NodeHandle, rpc::RethRpcAddOns};
use reth_node_core::args::RpcServerArgs;
use reth_rpc_builder::RpcModuleSelection;
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tempo_chainspec::spec::TempoChainSpec;
use tempo_primitives::TempoHeader;
use zone::{
    Deposit, DepositQueue, EncryptedDeposit, L1Deposit, L1PortalEvents, SharedL1StateCache,
    ZoneNode,
};

use alloy_provider::{Provider, ProviderBuilder, WalletProvider};
use alloy_rpc_types_eth::BlockNumberOrTag;
use alloy_signer_local::{MnemonicBuilder, coins_bip39::English};
use tempo_alloy::TempoNetwork;

/// Atomic counter for unique chain IDs across concurrent tests.
static NEXT_CHAIN_ID: AtomicU64 = AtomicU64::new(71_000);

fn next_unique_chain_id() -> u64 {
    NEXT_CHAIN_ID.fetch_add(1, Ordering::Relaxed)
}

/// Default timeout for polling loops in e2e tests.
pub(crate) const DEFAULT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Default poll interval for e2e tests.
pub(crate) const DEFAULT_POLL: std::time::Duration = std::time::Duration::from_millis(200);

pub(crate) const TEST_MNEMONIC: &str =
    "test test test test test test test test test test test junk";

/// Deterministic salt for the zone test token.
pub(crate) const ZONE_TEST_TOKEN_SALT: B256 = B256::new([
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
]);

/// Read a Foundry artifact from `docs/specs/out` and return its deployment bytecode.
///
/// Requires `forge build` to have been run in `docs/specs`.
fn forge_bytecode(contract: &str) -> eyre::Result<alloy_primitives::Bytes> {
    let specs_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/specs/out");
    let path = specs_dir.join(format!("{contract}.sol/{contract}.json"));
    let json = std::fs::read_to_string(&path).wrap_err_with(|| {
        format!("{contract} artifact not found – run `forge build` in docs/specs")
    })?;
    let artifact: serde_json::Value = serde_json::from_str(&json)?;
    let hex_str = artifact["bytecode"]["object"]
        .as_str()
        .ok_or_else(|| eyre::eyre!("missing bytecode in {contract} artifact"))?;
    Ok(alloy_primitives::Bytes::from(
        alloy_primitives::hex::decode(hex_str)?,
    ))
}

/// Dummy L1 URL used when no real L1 is needed.
///
/// Uses HTTP (not WS) because HTTP providers are lazy — they don't attempt a
/// connection until the first request, so `L1StateProvider::new` succeeds
/// without a running L1. The L1Subscriber will fail and retry in the background.
const DUMMY_L1_URL: &str = "http://127.0.0.1:1";

/// Compute the TIP-20 token address for a given sender and salt.
///
/// Mirrors `compute_tip20_address` in the factory precompile.
pub(crate) fn compute_tip20_address(sender: Address, salt: B256) -> Address {
    let hash = keccak256((sender, salt).abi_encode());

    let tip20_prefix: [u8; 12] = [
        0x20, 0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];

    let mut address_bytes = [0u8; 20];
    address_bytes[..12].copy_from_slice(&tip20_prefix);
    address_bytes[12..].copy_from_slice(&hash[..8]);

    Address::from(address_bytes)
}

pub(crate) trait TestNodeHandle: Send {}

impl<Node, AddOns> TestNodeHandle for NodeHandle<Node, AddOns>
where
    Node: FullNodeComponents,
    AddOns: RethRpcAddOns<Node>,
{
}

/// A self-contained Tempo Zone L2 node for integration testing.
///
/// Wraps an in-process reth node configured as a Zone, providing:
/// - An HTTP RPC endpoint for provider connections
/// - A [`DepositQueue`] handle for injecting synthetic L1 blocks
/// - A [`SharedL1StateCache`] for seeding TempoStateReader precompile data
///
/// # Construction
///
/// Use one of the static constructors depending on your test scenario:
///
/// - [`start_local()`](Self::start_local) — standalone node, no real L1, fastest for unit-style e2e
/// - [`start_local_with_chain_id()`](Self::start_local_with_chain_id) — standalone with custom chain ID (multi-zone tests)
/// - [`start_from_l1()`](Self::start_from_l1) — connected to a real [`L1TestNode`], genesis patched from L1 header
/// - [`start()`](Self::start) — connected to an external L1 via WebSocket URL
pub(crate) struct ZoneTestNode {
    http_url: url::Url,
    deposit_queue: DepositQueue,
    l1_state_cache: SharedL1StateCache,
    rpc_api: Arc<dyn zone::rpc::ZoneRpcApi>,
    canon_state_tx: reth_provider::CanonStateNotificationSender<tempo_primitives::TempoPrimitives>,
    _node_handle: Box<dyn TestNodeHandle>,
    _tasks: TaskManager,
}

impl ZoneTestNode {
    /// Returns the HTTP RPC URL for connecting providers to this node.
    pub(crate) fn http_url(&self) -> &url::Url {
        &self.http_url
    }

    /// Returns an HTTP provider connected to this zone node.
    pub(crate) fn provider(&self) -> alloy_provider::DynProvider {
        ProviderBuilder::new()
            .connect_http(self.http_url.clone())
            .erased()
    }

    /// Returns a handle to the deposit queue for injecting synthetic L1 blocks.
    pub(crate) fn deposit_queue(&self) -> &DepositQueue {
        &self.deposit_queue
    }

    /// Returns a handle to the L1 state cache for seeding precompile data.
    pub(crate) fn l1_state_cache(&self) -> &SharedL1StateCache {
        &self.l1_state_cache
    }

    /// Returns the real private RPC API backed by the node's EthHandlers.
    pub(crate) fn rpc_api(&self) -> Arc<dyn zone::rpc::ZoneRpcApi> {
        self.rpc_api.clone()
    }

    /// Subscribe to canonical state notifications.
    pub(crate) fn subscribe_to_canonical_state(
        &self,
    ) -> reth_provider::CanonStateNotifications<tempo_primitives::TempoPrimitives> {
        self.canon_state_tx.subscribe()
    }

    /// Wait for a TIP-20 token balance to reach at least `min_balance` on this zone.
    ///
    /// Polls the token's `balanceOf` until `balance >= min_balance`, then
    /// returns the observed balance. Useful for verifying deposit mints.
    ///
    /// **Important:** passing `U256::ZERO` returns immediately (any balance satisfies `>= 0`).
    /// Use the expected post-deposit balance as `min_balance` to actually wait.
    pub(crate) async fn wait_for_balance(
        &self,
        token: Address,
        account: Address,
        min_balance: U256,
        timeout: Duration,
    ) -> eyre::Result<U256> {
        use tempo_contracts::precompiles::ITIP20;

        let tip20 = ITIP20::new(token, self.provider());
        poll_until(timeout, DEFAULT_POLL, "token balance", || {
            let tip20 = &tip20;
            async move {
                let balance = tip20.balanceOf(account).call().await?;
                if balance >= min_balance {
                    Ok(Some(balance))
                } else {
                    Ok(None)
                }
            }
        })
        .await
    }

    /// Wait for `tempoBlockNumber` on this zone to reach at least `target`.
    ///
    /// Returns the observed block number once it reaches the target.
    pub(crate) async fn wait_for_tempo_block_number(
        &self,
        target: u64,
        timeout: Duration,
    ) -> eyre::Result<u64> {
        use zone::abi::{TEMPO_STATE_ADDRESS, TempoState};

        let tempo_state = TempoState::new(TEMPO_STATE_ADDRESS, self.provider());
        poll_until(
            timeout,
            DEFAULT_POLL,
            &format!("tempoBlockNumber >= {target}"),
            || {
                let tempo_state = &tempo_state;
                async move {
                    let n = tempo_state.tempoBlockNumber().call().await?;
                    if n >= target { Ok(Some(n)) } else { Ok(None) }
                }
            },
        )
        .await
    }

    /// Read a TIP-20 token balance on this zone (single-shot, no polling).
    pub(crate) async fn balance_of(&self, token: Address, account: Address) -> eyre::Result<U256> {
        use tempo_contracts::precompiles::ITIP20;
        Ok(ITIP20::new(token, self.provider())
            .balanceOf(account)
            .call()
            .await?)
    }

    /// Create a TIP-20 token on the zone L2 and grant `ISSUER_ROLE` to system contracts.
    ///
    /// Grants `ISSUER_ROLE` to `ZoneInbox` (minting deposits via `advanceTempo`) and
    /// `ZoneOutbox` (burning withdrawals). Must be done before deposits of this token
    /// can be processed. Uses the dev account (mnemonic index 0) which is pre-funded
    /// with pathUSD for gas.
    ///
    /// Returns the L2 token address.
    pub(crate) async fn create_l2_token(
        &self,
        name: &str,
        symbol: &str,
        salt: B256,
    ) -> eyre::Result<Address> {
        use tempo_chainspec::spec::TEMPO_T0_BASE_FEE;
        use tempo_contracts::precompiles::{IRolesAuth, ITIP20Factory};
        use tempo_precompiles::{PATH_USD_ADDRESS, TIP20_FACTORY_ADDRESS, tip20::ISSUER_ROLE};

        let signer = MnemonicBuilder::<English>::default()
            .phrase(TEST_MNEMONIC)
            .build()
            .expect("valid test mnemonic");
        let provider = ProviderBuilder::new()
            .wallet(signer)
            .connect_http(self.http_url.clone());

        // Create the token on L2
        let factory = ITIP20Factory::new(TIP20_FACTORY_ADDRESS, &provider);
        let receipt = factory
            .createToken(
                name.to_string(),
                symbol.to_string(),
                "USD".to_string(),
                PATH_USD_ADDRESS,
                provider.default_signer_address(),
                salt,
            )
            .gas_price(TEMPO_T0_BASE_FEE as u128)
            .gas(500_000)
            .send()
            .await?
            .get_receipt()
            .await?;
        eyre::ensure!(receipt.status(), "L2 createToken failed");

        let event = receipt
            .inner
            .logs()
            .iter()
            .find_map(|log| ITIP20Factory::TokenCreated::decode_log(&log.inner).ok())
            .ok_or_else(|| eyre::eyre!("TokenCreated event not found on L2"))?;
        let l2_token = event.token;

        // Grant ISSUER_ROLE to ZoneInbox so advanceTempo can mint deposits
        let roles = IRolesAuth::new(l2_token, &provider);
        let receipt = roles
            .grantRole(*ISSUER_ROLE, zone::abi::ZONE_INBOX_ADDRESS)
            .gas_price(TEMPO_T0_BASE_FEE as u128)
            .gas(300_000)
            .send()
            .await?
            .get_receipt()
            .await?;
        eyre::ensure!(receipt.status(), "L2 grantRole (ZoneInbox) failed");

        // Grant ISSUER_ROLE to ZoneOutbox so withdrawal burns work
        let receipt = roles
            .grantRole(*ISSUER_ROLE, zone::abi::ZONE_OUTBOX_ADDRESS)
            .gas_price(TEMPO_T0_BASE_FEE as u128)
            .gas(300_000)
            .send()
            .await?
            .get_receipt()
            .await?;
        eyre::ensure!(receipt.status(), "L2 grantRole (ZoneOutbox) failed");

        Ok(l2_token)
    }

    /// Wait for the zone L2 to finalize an L1 block beyond `after_block`.
    ///
    /// Polls for [`TempoState::TempoBlockFinalized`] logs on the zone L2 until
    /// one appears with a `blockNumber > after_block`, then confirms the on-chain
    /// `tempoBlockNumber` matches. Returns the finalized block number.
    ///
    /// Use this instead of manually polling `tempoBlockNumber()` — it's both
    /// event-driven (checks logs each iteration) and verifies consistency.
    pub(crate) async fn wait_for_l2_tempo_finalized(
        &self,
        after_block: u64,
        timeout: Duration,
    ) -> eyre::Result<u64> {
        use zone::abi::{TEMPO_STATE_ADDRESS, TempoState};

        let provider = self.provider();
        let tempo_state = TempoState::new(TEMPO_STATE_ADDRESS, &provider);

        let filter = Filter::new()
            .address(TEMPO_STATE_ADDRESS)
            .event_signature(TempoState::TempoBlockFinalized::SIGNATURE_HASH);

        poll_until(
            timeout,
            DEFAULT_POLL,
            "TempoBlockFinalized past target",
            || {
                let provider = &provider;
                let tempo_state = &tempo_state;
                let filter = &filter;
                async move {
                    // Check logs first — fast path when events already emitted
                    let logs = provider.get_logs(filter).await?;
                    for log in logs.iter().rev() {
                        if let Ok(ev) = TempoState::TempoBlockFinalized::decode_log(&log.inner)
                            && ev.blockNumber > after_block
                        {
                            // Confirm on-chain state matches
                            let on_chain = tempo_state.tempoBlockNumber().call().await?;
                            if on_chain >= ev.blockNumber {
                                return Ok(Some(on_chain));
                            }
                        }
                    }
                    Ok(None)
                }
            },
        )
        .await
    }

    /// Start a zone node pointing at a real L1 WebSocket URL.
    pub(crate) async fn start(l1_ws_url: String, portal_address: Address) -> eyre::Result<Self> {
        Self::launch(l1_ws_url, portal_address, None, next_unique_chain_id()).await
    }

    /// Start a zone node connected to a real L1, generating genesis from the L1's
    /// current block header.
    ///
    /// See [`build_l1_anchored_genesis`] for details on how the genesis is patched.
    pub(crate) async fn start_from_l1(
        l1_http_url: &url::Url,
        l1_ws_url: &url::Url,
        portal_address: Address,
    ) -> eyre::Result<Self> {
        let (genesis, genesis_block_number) =
            build_l1_anchored_genesis(l1_http_url, portal_address).await?;

        let throwaway_key = k256::SecretKey::from_slice(&[0x01; 32]).expect("valid throwaway key");
        Self::launch_with_genesis(
            l1_ws_url.to_string(),
            portal_address,
            Some(genesis_block_number),
            next_unique_chain_id(),
            Some(genesis),
            throwaway_key,
        )
        .await
    }

    /// Start a zone node connected to a real L1, with a sequencer key for ECIES decryption.
    ///
    /// Same as [`start_from_l1`] but passes the sequencer key through to `ZoneNode::new`
    /// so the payload builder can decrypt encrypted deposits.
    pub(crate) async fn start_from_l1_with_sequencer_key(
        l1_http_url: &url::Url,
        l1_ws_url: &url::Url,
        portal_address: Address,
        sequencer_key: k256::SecretKey,
    ) -> eyre::Result<Self> {
        let (genesis, genesis_block_number) =
            build_l1_anchored_genesis(l1_http_url, portal_address).await?;

        Self::launch_with_genesis(
            l1_ws_url.to_string(),
            portal_address,
            Some(genesis_block_number),
            next_unique_chain_id(),
            Some(genesis),
            sequencer_key,
        )
        .await
    }

    /// Start a self-contained zone node with no real L1 connection.
    ///
    /// The L1Subscriber retries a dummy URL in the background, but the
    /// ZoneEngine is fully functional. Deposits and L1 headers are injected
    /// directly into the `deposit_queue`; the L1 state cache must be seeded
    /// via [`L1Fixture::seed_l1_cache`] for TempoStateReader precompile reads.
    pub(crate) async fn start_local() -> eyre::Result<Self> {
        Self::launch(
            DUMMY_L1_URL.to_string(),
            Address::ZERO,
            None,
            next_unique_chain_id(),
        )
        .await
    }

    /// Start a self-contained zone node with a custom chain ID.
    ///
    /// Useful for running multiple zone nodes in a single test — each needs
    /// a unique chain ID to avoid datadir collisions.
    pub(crate) async fn start_local_with_chain_id(chain_id: u64) -> eyre::Result<Self> {
        Self::launch(DUMMY_L1_URL.to_string(), Address::ZERO, None, chain_id).await
    }

    async fn launch(
        l1_ws_url: String,
        portal_address: Address,
        genesis_tempo_block_number: Option<u64>,
        chain_id: u64,
    ) -> eyre::Result<Self> {
        // Generate a throwaway key for tests that don't use encrypted deposits.
        let throwaway_key = k256::SecretKey::from_slice(&[0x01; 32]).expect("valid throwaway key");
        Self::launch_with_genesis(
            l1_ws_url,
            portal_address,
            genesis_tempo_block_number,
            chain_id,
            None,
            throwaway_key,
        )
        .await
    }

    async fn launch_with_genesis(
        l1_ws_url: String,
        portal_address: Address,
        genesis_tempo_block_number: Option<u64>,
        chain_id: u64,
        custom_genesis: Option<Genesis>,
        sequencer_key: k256::SecretKey,
    ) -> eyre::Result<Self> {
        let tasks = TaskManager::current();

        let mut genesis = custom_genesis.unwrap_or_else(|| {
            serde_json::from_str(include_str!("../assets/zone-test-genesis.json"))
                .expect("valid zone test genesis")
        });
        genesis.config.chain_id = chain_id;
        let chain_spec = TempoChainSpec::from_genesis(genesis);

        let zone_node = ZoneNode::new(
            l1_ws_url,
            portal_address,
            genesis_tempo_block_number,
            Address::ZERO, // sequencer address (overridden by sequencer_key)
            sequencer_key,
        );

        // Don't use .dev() — it spawns a LocalMiner that conflicts with ZoneEngine.
        // The ZoneEngine is the sole block producer; it advances the chain when L1
        // blocks arrive in the deposit queue.
        let node_config = NodeConfig::new(Arc::new(chain_spec))
            .with_unused_ports()
            .with_rpc(
                RpcServerArgs::default()
                    .with_unused_ports()
                    .with_http()
                    .with_http_api(RpcModuleSelection::All),
            )
            .apply(|mut c| {
                c.network.discovery.disable_discovery = true;
                c
            });

        let deposit_queue = zone_node.deposit_queue();
        let l1_state_cache = zone_node.l1_state_cache();

        let node_handle = NodeBuilder::new(node_config)
            .testing_node(tasks.executor())
            .node(zone_node)
            .launch_with_debug_capabilities()
            .await?;

        let http_url = node_handle
            .node
            .rpc_server_handle()
            .http_url()
            .unwrap()
            .parse()
            .unwrap();

        // Build the real private RPC API while the handle is still concrete,
        // before type-erasing it into Box<dyn TestNodeHandle>.
        let eth_handlers = node_handle.node.eth_handlers().clone();
        let rpc_api: Arc<dyn zone::rpc::ZoneRpcApi> =
            Arc::new(zone::rpc::TempoZoneRpc::new(eth_handlers));

        // Create a relay broadcast channel for canon state notifications.
        // We subscribe to the node's provider and forward into our own sender
        // so test code can subscribe after the concrete node type is erased.
        let (canon_state_tx, _) = tokio::sync::broadcast::channel(64);
        {
            use reth_provider::CanonStateSubscriptions;
            let mut rx = node_handle.node.provider().subscribe_to_canonical_state();
            let tx = canon_state_tx.clone();
            tokio::spawn(async move {
                while let Ok(notification) = rx.recv().await {
                    let _ = tx.send(notification);
                }
            });
        }

        Ok(Self {
            deposit_queue,
            http_url,
            l1_state_cache,
            rpc_api,
            canon_state_tx,
            _node_handle: Box::new(node_handle),
            _tasks: tasks,
        })
    }
}

/// A Tempo L1 node running in dev mode for integration testing.
///
/// Starts an in-process Tempo node that produces blocks automatically
/// (500ms block time), providing both HTTP and WebSocket endpoints.
///
/// # Usage
///
/// ```ignore
/// let l1 = L1TestNode::start().await?;
/// let provider = ProviderBuilder::new().connect_http(l1.http_url().clone());
/// let zone = ZoneTestNode::start_from_l1(l1.http_url(), l1.ws_url(), Address::ZERO).await?;
/// ```
pub(crate) struct L1TestNode {
    http_url: url::Url,
    ws_url: url::Url,
    _node_handle: Box<dyn TestNodeHandle>,
    _tasks: TaskManager,
}

impl L1TestNode {
    /// Returns the HTTP RPC URL for this L1 node.
    pub(crate) fn http_url(&self) -> &url::Url {
        &self.http_url
    }

    /// Returns the WebSocket RPC URL for this L1 node.
    pub(crate) fn ws_url(&self) -> &url::Url {
        &self.ws_url
    }

    /// Returns an unsigned HTTP provider connected to this L1 node.
    pub(crate) fn provider(&self) -> alloy_provider::DynProvider {
        ProviderBuilder::new()
            .connect_http(self.http_url.clone())
            .erased()
    }

    /// Returns a signer for the pre-funded dev account.
    ///
    /// This is the first key derived from [`TEST_MNEMONIC`] (`test test … junk`),
    /// corresponding to address `0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266`.
    /// The account is pre-funded with pathUSD in `test-genesis.json`.
    pub(crate) fn dev_signer(&self) -> alloy_signer_local::PrivateKeySigner {
        MnemonicBuilder::<English>::default()
            .phrase(TEST_MNEMONIC)
            .build()
            .expect("valid test mnemonic")
    }

    /// Returns the address of the pre-funded dev account.
    pub(crate) fn dev_address(&self) -> Address {
        self.dev_signer().address()
    }

    /// Returns a signer for the second test account (mnemonic index 1).
    ///
    /// This account is NOT pre-funded — use [`fund_user`](Self::fund_user) to
    /// transfer pathUSD from the dev account before depositing.
    pub(crate) fn user_signer(&self) -> alloy_signer_local::PrivateKeySigner {
        MnemonicBuilder::<English>::default()
            .phrase(TEST_MNEMONIC)
            .index(1)
            .expect("valid derivation index")
            .build()
            .expect("valid test mnemonic")
    }

    /// Returns a signer derived from [`TEST_MNEMONIC`] at the given BIP-44 index.
    pub(crate) fn signer_at(&self, index: u32) -> alloy_signer_local::PrivateKeySigner {
        MnemonicBuilder::<English>::default()
            .phrase(TEST_MNEMONIC)
            .index(index)
            .expect("valid derivation index")
            .build()
            .expect("valid test mnemonic")
    }

    /// Transfer pathUSD from the dev account to a recipient on L1.
    pub(crate) async fn fund_user(&self, to: Address, amount: u128) -> eyre::Result<()> {
        use tempo_contracts::precompiles::ITIP20;
        use tempo_precompiles::PATH_USD_ADDRESS;

        let provider = self.dev_provider();
        let receipt = ITIP20::new(PATH_USD_ADDRESS, &provider)
            .transfer(to, U256::from(amount))
            .send()
            .await?
            .get_receipt()
            .await?;
        eyre::ensure!(receipt.status(), "fund_user transfer failed");
        Ok(())
    }

    /// Read a TIP-20 token balance on L1 (single-shot, no polling).
    pub(crate) async fn balance_of(&self, token: Address, account: Address) -> eyre::Result<U256> {
        use tempo_contracts::precompiles::ITIP20;
        Ok(ITIP20::new(token, self.provider())
            .balanceOf(account)
            .call()
            .await?)
    }

    /// Wait for a TIP-20 token balance to reach at least `min_balance` on L1.
    pub(crate) async fn wait_for_balance(
        &self,
        token: Address,
        account: Address,
        min_balance: U256,
        timeout: Duration,
    ) -> eyre::Result<U256> {
        use tempo_contracts::precompiles::ITIP20;

        let tip20 = ITIP20::new(token, self.provider());
        poll_until(timeout, DEFAULT_POLL, "L1 token balance", || {
            let tip20 = &tip20;
            async move {
                let balance = tip20.balanceOf(account).call().await?;
                if balance >= min_balance {
                    Ok(Some(balance))
                } else {
                    Ok(None)
                }
            }
        })
        .await
    }

    /// Assert that a `BatchSubmitted` event exists on the portal.
    pub(crate) async fn assert_batch_submitted(&self, portal_address: Address) -> eyre::Result<()> {
        use zone::abi::ZonePortal;
        let portal = ZonePortal::new(portal_address, self.provider());
        let events = portal.BatchSubmitted_filter().from_block(0).query().await?;
        eyre::ensure!(
            !events.is_empty(),
            "expected at least one BatchSubmitted event on L1"
        );
        Ok(())
    }

    /// Assert that a `WithdrawalProcessed` event exists on the portal matching `to` and `amount`.
    pub(crate) async fn assert_withdrawal_processed(
        &self,
        portal_address: Address,
        to: Address,
        amount: u128,
    ) -> eyre::Result<()> {
        use zone::abi::ZonePortal;
        let portal = ZonePortal::new(portal_address, self.provider());
        let events = portal
            .WithdrawalProcessed_filter()
            .from_block(0)
            .query()
            .await?;
        let found = events.iter().any(|(e, _)| e.to == to && e.amount == amount);
        eyre::ensure!(
            found,
            "expected WithdrawalProcessed event for {to} with amount {amount}"
        );
        Ok(())
    }

    /// Returns an HTTP provider with the dev account wallet attached.
    pub(crate) fn dev_provider(&self) -> alloy_provider::DynProvider {
        ProviderBuilder::new()
            .wallet(self.dev_signer())
            .connect_http(self.http_url.clone())
            .erased()
    }

    /// Deploy the ZoneFactory and create a zone in one step.
    ///
    /// Combines [`deploy_zone_factory`](Self::deploy_zone_factory) and
    /// [`create_zone`](Self::create_zone). Returns the portal address.
    pub(crate) async fn deploy_zone(&self) -> eyre::Result<Address> {
        let factory = self.deploy_zone_factory().await?;
        self.create_zone(factory).await
    }

    /// Deposit pathUSD from the dev account into a zone for L2 gas.
    pub(crate) async fn fund_dev_l2_gas(
        &self,
        portal_address: Address,
        zone: &ZoneTestNode,
        amount: u128,
        timeout: Duration,
    ) -> eyre::Result<()> {
        use tempo_contracts::precompiles::ITIP20;
        use tempo_precompiles::PATH_USD_ADDRESS;
        use zone::abi::{ZONE_TOKEN_ADDRESS, ZonePortal};

        let dev_provider = self.dev_provider();
        ITIP20::new(PATH_USD_ADDRESS, &dev_provider)
            .approve(portal_address, U256::MAX)
            .send()
            .await?
            .get_receipt()
            .await?;
        let receipt = ZonePortal::new(portal_address, &dev_provider)
            .deposit(PATH_USD_ADDRESS, self.dev_address(), amount, B256::ZERO)
            .send()
            .await?
            .get_receipt()
            .await?;
        eyre::ensure!(receipt.status(), "dev L2 gas deposit failed");
        zone.wait_for_balance(
            ZONE_TOKEN_ADDRESS,
            self.dev_address(),
            U256::from(amount),
            timeout,
        )
        .await?;
        Ok(())
    }

    /// Wait for a withdrawal to be fully processed on L1 (pathUSD).
    ///
    /// Polls the account's L1 token balance until it increases by at least
    /// `amount`, then asserts both `BatchSubmitted` and `WithdrawalProcessed`
    /// events exist on the portal.
    pub(crate) async fn wait_for_withdrawal_on_l1(
        &self,
        portal_address: Address,
        account: Address,
        amount: u128,
        timeout: Duration,
    ) -> eyre::Result<()> {
        use tempo_precompiles::PATH_USD_ADDRESS;
        self.wait_for_withdrawal_on_l1_token(
            portal_address,
            PATH_USD_ADDRESS,
            account,
            amount,
            timeout,
        )
        .await
    }

    /// Wait for a withdrawal of a specific token to be fully processed on L1.
    pub(crate) async fn wait_for_withdrawal_on_l1_token(
        &self,
        portal_address: Address,
        token: Address,
        account: Address,
        amount: u128,
        timeout: Duration,
    ) -> eyre::Result<()> {
        let balance_before = self.balance_of(token, account).await?;
        let expected = balance_before + U256::from(amount);
        self.wait_for_balance(token, account, expected, timeout)
            .await?;
        self.assert_batch_submitted(portal_address).await?;
        self.assert_withdrawal_processed(portal_address, account, amount)
            .await
    }

    /// Deploy the ZoneFactory contract on L1 from the Foundry artifact.
    ///
    /// The factory constructor also deploys a Verifier internally.
    /// Returns the factory address for use with [`create_zone`](Self::create_zone).
    pub(crate) async fn deploy_zone_factory(&self) -> eyre::Result<Address> {
        use alloy_primitives::TxKind;
        use alloy_rpc_types_eth::TransactionRequest;

        let l1_provider = self.dev_provider();

        let bytecode = forge_bytecode("ZoneFactory")?;

        let mut deploy_tx = TransactionRequest::default().input(bytecode.into());
        deploy_tx.to = Some(TxKind::Create);
        let receipt = l1_provider
            .send_transaction(deploy_tx)
            .await?
            .get_receipt()
            .await?;
        eyre::ensure!(receipt.status(), "ZoneFactory deployment failed");

        receipt
            .contract_address
            .ok_or_else(|| eyre::eyre!("ZoneFactory deployment missing contract address"))
    }

    /// Create a zone on an existing ZoneFactory and return the portal address.
    ///
    /// Captures the current L1 header as the genesis anchor, then calls
    /// `createZone()` with pathUSD as the token and the dev account as sequencer.
    pub(crate) async fn create_zone(&self, factory_address: Address) -> eyre::Result<Address> {
        self.create_zone_with_sequencer(factory_address, self.dev_address())
            .await
    }

    /// Create a zone on an existing ZoneFactory with a custom sequencer address.
    pub(crate) async fn create_zone_with_sequencer(
        &self,
        factory_address: Address,
        sequencer: Address,
    ) -> eyre::Result<Address> {
        use tempo_precompiles::PATH_USD_ADDRESS;
        use zone::abi::ZoneFactory;

        let l1_provider = self.dev_provider();
        let factory = ZoneFactory::new(factory_address, &l1_provider);

        // Capture genesis anchor from the current L1 header
        let l1_tempo_provider =
            ProviderBuilder::new_with_network::<TempoNetwork>().connect_http(self.http_url.clone());
        let block = l1_tempo_provider
            .get_block_by_number(BlockNumberOrTag::Latest)
            .await?
            .ok_or_else(|| eyre::eyre!("L1 latest block not found"))?;
        let l1_header: &TempoHeader = block.header.as_ref();

        let mut rlp_buf = Vec::new();
        l1_header.encode(&mut rlp_buf);
        let genesis_tempo_block_hash = keccak256(&rlp_buf);

        let verifier_address = factory.verifier().call().await?;
        let receipt = factory
            .createZone(ZoneFactory::CreateZoneParams {
                token: PATH_USD_ADDRESS,
                sequencer,
                verifier: verifier_address,
                zoneParams: ZoneFactory::ZoneParams {
                    genesisBlockHash: B256::ZERO,
                    genesisTempoBlockHash: genesis_tempo_block_hash,
                    genesisTempoBlockNumber: l1_header.inner.number,
                },
            })
            .send()
            .await?
            .get_receipt()
            .await?;
        eyre::ensure!(receipt.status(), "createZone failed");

        let zone_created = receipt
            .inner
            .logs()
            .iter()
            .find_map(|log| ZoneFactory::ZoneCreated::decode_log(&log.inner).ok())
            .ok_or_else(|| eyre::eyre!("ZoneCreated event not found"))?;

        Ok(zone_created.portal)
    }

    /// Deploy the SwapAndDepositRouter contract on L1 from the Foundry artifact.
    ///
    /// The constructor takes `(address stablecoinDEX, address zoneFactory)`.
    /// We pass `Address::ZERO` for the DEX since both zones use the same token.
    pub(crate) async fn deploy_router(&self, factory_address: Address) -> eyre::Result<Address> {
        self.deploy_router_with_dex(factory_address, Address::ZERO)
            .await
    }

    /// Deploy the SwapAndDepositRouter with a specific DEX address.
    ///
    /// Use this when the test requires actual token swaps via the StablecoinDEX.
    pub(crate) async fn deploy_router_with_dex(
        &self,
        factory_address: Address,
        dex_address: Address,
    ) -> eyre::Result<Address> {
        use alloy_primitives::{Bytes, TxKind};
        use alloy_rpc_types_eth::TransactionRequest;
        use alloy_sol_types::SolValue;

        let l1_provider = self.dev_provider();

        // Constructor: constructor(address _stablecoinDEX, address _zoneFactory)
        let mut deploy_bytes = forge_bytecode("SwapAndDepositRouter")?.to_vec();
        deploy_bytes.extend_from_slice(&(dex_address, factory_address).abi_encode());
        let bytecode = Bytes::from(deploy_bytes);

        let mut deploy_tx = TransactionRequest::default().input(bytecode.into());
        deploy_tx.to = Some(TxKind::Create);
        let receipt = l1_provider
            .send_transaction(deploy_tx)
            .await?
            .get_receipt()
            .await?;
        eyre::ensure!(receipt.status(), "SwapAndDepositRouter deployment failed");

        receipt
            .contract_address
            .ok_or_else(|| eyre::eyre!("SwapAndDepositRouter deployment missing contract address"))
    }

    /// Deploy L1 infrastructure for a two-zone cross-zone test with separate sequencers.
    pub(crate) async fn deploy_two_zones_with_sequencers(
        &self,
        sequencer_a: Address,
        sequencer_b: Address,
    ) -> eyre::Result<(Address, Address, Address)> {
        let factory = self.deploy_zone_factory().await?;
        let portal_a = self
            .create_zone_with_sequencer(factory, sequencer_a)
            .await?;
        let portal_b = self
            .create_zone_with_sequencer(factory, sequencer_b)
            .await?;
        let router = self.deploy_router(factory).await?;
        Ok((portal_a, portal_b, router))
    }

    /// Create a new TIP-20 token on L1 via the factory precompile.
    ///
    /// Returns the new token's address.
    pub(crate) async fn create_tip20(
        &self,
        name: &str,
        symbol: &str,
        salt: B256,
    ) -> eyre::Result<Address> {
        use alloy_sol_types::SolEvent;
        use tempo_contracts::precompiles::ITIP20Factory;
        use tempo_precompiles::{PATH_USD_ADDRESS, TIP20_FACTORY_ADDRESS};

        let provider = self.dev_provider();
        let factory = ITIP20Factory::new(TIP20_FACTORY_ADDRESS, &provider);
        let receipt = factory
            .createToken(
                name.to_string(),
                symbol.to_string(),
                "USD".to_string(),
                PATH_USD_ADDRESS,
                self.dev_address(),
                salt,
            )
            .send()
            .await?
            .get_receipt()
            .await?;
        eyre::ensure!(receipt.status(), "createToken failed");

        let event = receipt
            .inner
            .logs()
            .iter()
            .find_map(|log| ITIP20Factory::TokenCreated::decode_log(&log.inner).ok())
            .ok_or_else(|| eyre::eyre!("TokenCreated event not found"))?;

        Ok(event.token)
    }

    /// Enable a token on a ZonePortal (must be called by the sequencer).
    pub(crate) async fn enable_token_on_portal(
        &self,
        portal_address: Address,
        token: Address,
    ) -> eyre::Result<()> {
        use zone::abi::ZonePortal;
        let provider = self.dev_provider();
        let portal = ZonePortal::new(portal_address, &provider);
        let receipt = portal
            .enableToken(token)
            .send()
            .await?
            .get_receipt()
            .await?;
        eyre::ensure!(receipt.status(), "enableToken failed");
        Ok(())
    }

    /// Set the sequencer encryption key on the ZonePortal.
    ///
    /// The sequencer must sign a proof-of-possession with the encryption key's
    /// private key. The POP message is `keccak256(abi.encode(portalAddress, x, yParity))`.
    pub(crate) async fn set_sequencer_encryption_key(
        &self,
        portal_address: Address,
        encryption_key: &k256::SecretKey,
    ) -> eyre::Result<()> {
        use alloy_signer::SignerSync;
        use k256::{AffinePoint, ProjectivePoint, Scalar, elliptic_curve::sec1::ToEncodedPoint};
        use zone::abi::ZonePortal;

        // Derive public key coordinates
        let scalar: Scalar = *encryption_key.to_nonzero_scalar();
        let pub_point = AffinePoint::from(ProjectivePoint::GENERATOR * scalar);
        let encoded = pub_point.to_encoded_point(true);
        let x = B256::from_slice(encoded.x().unwrap().as_slice());
        let y_parity: u8 = encoded.as_bytes()[0]; // 0x02 or 0x03

        // Build POP message matching Solidity: keccak256(abi.encode(address(this), x, yParity))
        // yParity is uint8 in Solidity, which abi.encode pads to 32 bytes — use U256
        let message = keccak256((portal_address, x, U256::from(y_parity)).abi_encode());

        // Sign with the encryption key (not the sequencer's Ethereum key)
        let enc_key_bytes = B256::from_slice(&encryption_key.to_bytes());
        let pop_signer = alloy_signer_local::PrivateKeySigner::from_bytes(&enc_key_bytes)?;
        let sig = pop_signer.sign_hash_sync(&message)?;

        // ecrecover expects v = 27 or 28
        let pop_v = sig.v() as u8 + 27;
        let pop_r = B256::from(sig.r().to_be_bytes::<32>());
        let pop_s = B256::from(sig.s().to_be_bytes::<32>());

        // Call setSequencerEncryptionKey as the sequencer (dev account)
        let dev_provider = self.dev_provider();
        let portal = ZonePortal::new(portal_address, &dev_provider);
        let receipt = portal
            .setSequencerEncryptionKey(x, y_parity, pop_v, pop_r, pop_s)
            .send()
            .await?
            .get_receipt()
            .await?;
        eyre::ensure!(receipt.status(), "setSequencerEncryptionKey failed");
        Ok(())
    }

    /// Transfer a specific TIP-20 token from the dev account to a recipient on L1.
    pub(crate) async fn fund_user_token(
        &self,
        token: Address,
        to: Address,
        amount: u128,
    ) -> eyre::Result<()> {
        use tempo_contracts::precompiles::ITIP20;
        let provider = self.dev_provider();
        let receipt = ITIP20::new(token, &provider)
            .transfer(to, U256::from(amount))
            .send()
            .await?
            .get_receipt()
            .await?;
        eyre::ensure!(receipt.status(), "fund_user_token transfer failed");
        Ok(())
    }

    /// Mint tokens on L1.
    ///
    /// The dev account must be the admin of the token (set during `createToken`).
    /// Grants `ISSUER_ROLE` to self first (admin can grant roles), then mints.
    pub(crate) async fn mint_tip20(
        &self,
        token: Address,
        to: Address,
        amount: u128,
    ) -> eyre::Result<()> {
        use tempo_contracts::precompiles::{IRolesAuth, ITIP20};
        use tempo_precompiles::tip20::ISSUER_ROLE;

        let provider = self.dev_provider();

        // Admin can grant ISSUER_ROLE to self
        let receipt = IRolesAuth::new(token, &provider)
            .grantRole(*ISSUER_ROLE, self.dev_address())
            .send()
            .await?
            .get_receipt()
            .await?;
        eyre::ensure!(receipt.status(), "grantRole ISSUER failed on L1");

        let receipt = ITIP20::new(token, &provider)
            .mint(to, U256::from(amount))
            .send()
            .await?
            .get_receipt()
            .await?;
        eyre::ensure!(receipt.status(), "mint_tip20 failed");
        Ok(())
    }

    /// Start an L1 dev node with the default configuration (500ms block time).
    pub(crate) async fn start() -> eyre::Result<Self> {
        Self::start_with(|_| {}).await
    }

    /// Start an L1 dev node, applying a closure to customise the [`NodeConfig`]
    /// before launch.
    ///
    /// The base config already has dev mode enabled, random ports, and full
    /// HTTP + WS RPC. The closure receives a `&mut NodeConfig` for last-mile
    /// tweaks (e.g. changing block time):
    ///
    /// ```ignore
    /// let l1 = L1TestNode::start_with(|cfg| {
    ///     cfg.dev.block_time = Some(Duration::from_secs(1));
    /// }).await?;
    /// ```
    pub(crate) async fn start_with(
        f: impl FnOnce(&mut NodeConfig<TempoChainSpec>),
    ) -> eyre::Result<Self> {
        let tasks = TaskManager::current();

        let genesis: serde_json::Value =
            serde_json::from_str(include_str!("../assets/test-genesis.json"))?;
        let chain_spec = TempoChainSpec::from_genesis(serde_json::from_value(genesis)?);

        let mut node_config = NodeConfig::new(Arc::new(chain_spec))
            .with_unused_ports()
            .dev()
            .with_rpc(
                RpcServerArgs::default()
                    .with_unused_ports()
                    .with_http()
                    .with_http_api(RpcModuleSelection::All)
                    .with_ws()
                    .with_ws_api(RpcModuleSelection::All),
            )
            .apply(|mut c| {
                c.dev.block_time = Some(Duration::from_millis(500));
                c
            });

        f(&mut node_config);

        let node_handle = NodeBuilder::new(node_config)
            .testing_node(tasks.executor())
            .node(tempo_node::node::TempoNode::default())
            .launch_with_debug_capabilities()
            .await?;

        let http_url = node_handle
            .node
            .rpc_server_handle()
            .http_url()
            .unwrap()
            .parse()
            .unwrap();
        let ws_url = node_handle
            .node
            .rpc_server_handle()
            .ws_url()
            .unwrap()
            .parse()
            .unwrap();

        Ok(Self {
            http_url,
            ws_url,
            _node_handle: Box::new(node_handle),
            _tasks: tasks,
        })
    }
}

/// Build a zone test genesis anchored to a real L1 block.
///
/// The base `zone-test-genesis.json` is a standalone genesis with:
/// - TempoState anchored at block 0 with a zero block hash
/// - ZoneInbox compiled with `tempoPortal = Address::ZERO` (Solidity immutable)
///
/// When connecting to a real L1, two things must be patched:
///
/// 1. **TempoState storage** — `tempoBlockHash` (slot 0) and the packed header fields
///    in slot 7 must reflect the L1 block that serves as the zone's genesis anchor.
///    Without this, `finalizeTempo` rejects the first L1 block for parent hash mismatch.
///
/// 2. **ZoneInbox bytecode** — the `tempoPortal` immutable (embedded in deployed bytecode
///    as `PUSH32` instructions) must be replaced with the real portal address. Without this,
///    `readTempoStorageSlot` reads L1 state from `Address::ZERO` instead of the portal,
///    causing `_readEncryptionKey` to revert with `InvalidSharedSecretProof`.
///
/// Returns `(genesis, genesis_block_number)`.
async fn build_l1_anchored_genesis(
    l1_http_url: &url::Url,
    portal_address: Address,
) -> eyre::Result<(Genesis, u64)> {
    use alloy_primitives::address;

    let l1_provider =
        ProviderBuilder::new_with_network::<TempoNetwork>().connect_http(l1_http_url.clone());

    let block = l1_provider
        .get_block_by_number(BlockNumberOrTag::Latest)
        .await?
        .ok_or_else(|| eyre::eyre!("L1 latest block not found"))?;
    let l1_header: &TempoHeader = block.header.as_ref();
    let genesis_block_number = l1_header.inner.number;

    let mut rlp_buf = Vec::new();
    l1_header.encode(&mut rlp_buf);
    let l1_genesis_hash = keccak256(&rlp_buf);

    let mut genesis: Genesis =
        serde_json::from_str(include_str!("../assets/zone-test-genesis.json"))?;

    // --- Patch 1: TempoState storage ---
    // TempoState is at 0x1c00...0000
    let tempo_state_addr = address!("0x1c00000000000000000000000000000000000000");
    let tempo_state_account = genesis
        .alloc
        .get_mut(&tempo_state_addr)
        .ok_or_else(|| eyre::eyre!("TempoState not found in genesis alloc"))?;
    let storage = tempo_state_account
        .storage
        .get_or_insert_with(Default::default);

    // Slot 0 = tempoBlockHash
    storage.insert(B256::ZERO, l1_genesis_hash);

    // Slot 7 = packed (tempoBlockNumber:u64 | tempoGasLimit:u64 | tempoGasUsed:u64 | tempoTimestamp:u64)
    let new_slot7: U256 = U256::from(l1_header.inner.number)
        | (U256::from(l1_header.inner.gas_limit) << 64)
        | (U256::from(l1_header.inner.gas_used) << 128)
        | (U256::from(l1_header.inner.timestamp) << 192);
    storage.insert(
        B256::from(U256::from(7).to_be_bytes()),
        B256::from(new_slot7.to_be_bytes()),
    );

    // --- Patch 2: Portal address immutables in ZoneInbox and ZoneConfig ---
    // Solidity immutables are baked into deployed bytecode as `PUSH32 <value>`.
    // The default genesis has tempoPortal = Address::ZERO. We replace the 32-byte
    // zero-padded needle at the byte level. Both ZoneInbox (0x...0001) and
    // ZoneConfig (0x...0003) have `tempoPortal` as an immutable.
    if !portal_address.is_zero() {
        let needle = [0u8; 32]; // Address::ZERO left-padded to 32 bytes
        let mut replacement = [0u8; 32];
        replacement[12..].copy_from_slice(portal_address.as_slice());

        let contracts_to_patch: &[(Address, usize)] = &[
            (address!("0x1c00000000000000000000000000000000000001"), 4), // ZoneInbox
            (address!("0x1c00000000000000000000000000000000000003"), 5), // ZoneConfig
        ];

        for &(addr, expected_count) in contracts_to_patch {
            let account = genesis
                .alloc
                .get_mut(&addr)
                .unwrap_or_else(|| panic!("contract {addr} missing in genesis alloc"));
            if let Some(code) = &account.code {
                let mut buf = code.to_vec();
                let count = patch_bytes(&mut buf, &needle, &replacement);
                assert_eq!(
                    count, expected_count,
                    "expected {expected_count} tempoPortal immutable(s) in {addr}, found {count} \
                     — contract bytecode may have changed, update expected_count"
                );
                account.code = Some(buf.into());
            }
        }
    }

    Ok((genesis, genesis_block_number))
}

/// Replace all non-overlapping occurrences of `needle` with `replacement` in `buf`.
///
/// Both must have the same length. Returns the number of replacements made.
fn patch_bytes(buf: &mut [u8], needle: &[u8], replacement: &[u8]) -> usize {
    assert_eq!(needle.len(), replacement.len());
    let len = needle.len();
    let mut count = 0;
    let mut i = 0;
    while i + len <= buf.len() {
        if buf[i..i + len] == *needle {
            buf[i..i + len].copy_from_slice(replacement);
            count += 1;
            i += len;
        } else {
            i += 1;
        }
    }
    count
}

/// Poll an async condition until it returns `Some(T)` or the timeout expires.
pub(crate) async fn poll_until<T, Fut, F>(
    timeout: std::time::Duration,
    interval: std::time::Duration,
    description: &str,
    mut f: F,
) -> eyre::Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = eyre::Result<Option<T>>>,
{
    let start = std::time::Instant::now();
    loop {
        if let Some(v) = f().await.wrap_err("poll iteration failed")? {
            return Ok(v);
        }
        if start.elapsed() > timeout {
            eyre::bail!("timed out after {timeout:?}: {description}");
        }
        tokio::time::sleep(interval).await;
    }
}

/// Arguments for [`ZoneAccount::withdraw_with`].
///
/// Use [`WithdrawalArgs::new`] for the common case (amount only, self-withdrawal),
/// then override individual fields as needed.
pub(crate) struct WithdrawalArgs {
    pub amount: u128,
    pub to: Option<Address>,
    pub memo: B256,
    pub gas_limit: u64,
    pub fallback_recipient: Option<Address>,
    pub data: alloy_primitives::Bytes,
}

impl WithdrawalArgs {
    /// Simple withdrawal: send `amount` back to self with no callback.
    pub(crate) fn new(amount: u128) -> Self {
        Self {
            amount,
            to: None,
            memo: B256::ZERO,
            gas_limit: 0,
            fallback_recipient: None,
            data: alloy_primitives::Bytes::new(),
        }
    }

    /// Cross-zone withdrawal via the [`SwapAndDepositRouter`].
    ///
    /// The withdrawal callback sends tokens to the router, which deposits them
    /// into `target_portal` for `recipient`. Both zones must use the same token
    /// (no swap needed — `tokenOut == tokenIn`).
    pub(crate) fn cross_zone_via_router(
        amount: u128,
        router: Address,
        target_portal: Address,
        token: Address,
        recipient: Address,
    ) -> Self {
        use alloy_sol_types::SolValue;

        // SwapAndDepositRouter plaintext callback format:
        // (bool isEncrypted, address tokenOut, address targetPortal, address recipient, bytes32 memo, uint128 minAmountOut)
        let callback_data =
            (false, token, target_portal, recipient, B256::ZERO, 0u128).abi_encode();

        Self {
            amount,
            to: Some(router),
            memo: B256::ZERO,
            gas_limit: 500_000,
            fallback_recipient: None, // defaults to self
            data: alloy_primitives::Bytes::from(callback_data),
        }
    }
}

/// A test account that can interact with both L1 and L2 (zone) nodes.
///
/// Wraps a signing key and provides high-level helpers for the common
/// deposit/withdrawal flow, tracking approvals to avoid redundant transactions.
pub(crate) struct ZoneAccount {
    /// The account's on-chain address (derived from `signer`).
    address: Address,
    /// Wallet-attached provider for Tempo L1 (deposits, approvals).
    l1_provider: alloy_provider::DynProvider,
    /// Wallet-attached provider for the Zone L2 (withdrawals, approvals).
    l2_provider: alloy_provider::DynProvider,
    /// The ZonePortal contract address on L1 for this zone.
    portal_address: Address,
    /// Whether we've already approved the portal to spend pathUSD on L1.
    l1_portal_approved: bool,
}

impl ZoneAccount {
    /// Create a new `ZoneAccount` from an [`L1TestNode`] and [`ZoneTestNode`].
    ///
    /// Uses the L1's **user** signer (mnemonic index 1) as the account key,
    /// separate from the dev/sequencer account (index 0). The same key signs
    /// both L1 and L2 transactions.
    ///
    /// The user account must be funded on L1 before depositing — call
    /// [`L1TestNode::fund_user`] first.
    pub(crate) fn from_l1_and_zone(
        l1: &L1TestNode,
        zone: &ZoneTestNode,
        portal_address: Address,
    ) -> Self {
        let signer = l1.user_signer();
        let address = signer.address();

        let l1_provider = ProviderBuilder::new()
            .wallet(signer.clone())
            .connect_http(l1.http_url().clone())
            .erased();

        let l2_provider = ProviderBuilder::new()
            .wallet(signer)
            .connect_http(zone.http_url().clone())
            .erased();

        Self {
            address,
            l1_provider,
            l2_provider,
            portal_address,
            l1_portal_approved: false,
        }
    }

    /// Create a `ZoneAccount` with a custom signer.
    ///
    /// Unlike [`from_l1_and_zone`](Self::from_l1_and_zone) which uses the L1's
    /// user signer, this allows creating an account with any private key —
    /// useful when the account was funded via encrypted deposit to a specific
    /// recipient.
    pub(crate) fn with_signer(
        signer: alloy_signer_local::PrivateKeySigner,
        l1: &L1TestNode,
        zone: &ZoneTestNode,
        portal_address: Address,
    ) -> Self {
        let address = signer.address();

        let l1_provider = ProviderBuilder::new()
            .wallet(signer.clone())
            .connect_http(l1.http_url().clone())
            .erased();

        let l2_provider = ProviderBuilder::new()
            .wallet(signer)
            .connect_http(zone.http_url().clone())
            .erased();

        Self {
            address,
            l1_provider,
            l2_provider,
            portal_address,
            l1_portal_approved: false,
        }
    }

    /// The account's address.
    pub(crate) fn address(&self) -> Address {
        self.address
    }

    /// Approve the ZonePortal to spend pathUSD on L1, then deposit.
    ///
    /// Skips approval if already approved in this session.
    /// Waits for the expected post-deposit balance on L2 and returns it.
    pub(crate) async fn deposit(
        &mut self,
        amount: u128,
        timeout: Duration,
        zone: &ZoneTestNode,
    ) -> eyre::Result<U256> {
        use tempo_contracts::precompiles::ITIP20;
        use tempo_precompiles::PATH_USD_ADDRESS;
        use zone::abi::{ZONE_TOKEN_ADDRESS, ZonePortal};

        if !self.l1_portal_approved {
            ITIP20::new(PATH_USD_ADDRESS, &self.l1_provider)
                .approve(self.portal_address, U256::MAX)
                .send()
                .await?
                .get_receipt()
                .await?;
            self.l1_portal_approved = true;
        }

        // Snapshot balance before deposit so we wait for the expected increase
        let balance_before = zone.balance_of(ZONE_TOKEN_ADDRESS, self.address).await?;

        let portal = ZonePortal::new(self.portal_address, &self.l1_provider);
        let receipt = portal
            .deposit(PATH_USD_ADDRESS, self.address, amount, B256::ZERO)
            .send()
            .await?
            .get_receipt()
            .await?;
        eyre::ensure!(receipt.status(), "L1 deposit tx failed");

        zone.wait_for_balance(
            ZONE_TOKEN_ADDRESS,
            self.address,
            balance_before + U256::from(amount),
            timeout,
        )
        .await
    }

    /// Approve the ZonePortal to spend `amount` of a specific `token` on L1, then deposit.
    ///
    /// Unlike [`deposit`](Self::deposit), this allows depositing any enabled token.
    /// The caller must ensure:
    /// - The token is enabled on the portal (`enableToken`)
    /// - The account has sufficient balance of `token` on L1
    ///
    /// Waits for the expected post-deposit balance on L2 and returns it.
    pub(crate) async fn deposit_token(
        &mut self,
        token: Address,
        l2_token: Address,
        amount: u128,
        timeout: Duration,
        zone: &ZoneTestNode,
    ) -> eyre::Result<U256> {
        use tempo_contracts::precompiles::ITIP20;
        use zone::abi::ZonePortal;

        // Approve portal for this specific token
        ITIP20::new(token, &self.l1_provider)
            .approve(self.portal_address, U256::MAX)
            .send()
            .await?
            .get_receipt()
            .await?;

        // Snapshot balance before deposit so we wait for the expected increase
        let balance_before = zone.balance_of(l2_token, self.address).await?;

        let portal = ZonePortal::new(self.portal_address, &self.l1_provider);
        let receipt = portal
            .deposit(token, self.address, amount, B256::ZERO)
            .send()
            .await?
            .get_receipt()
            .await?;
        eyre::ensure!(receipt.status(), "L1 deposit tx failed");

        zone.wait_for_balance(
            l2_token,
            self.address,
            balance_before + U256::from(amount),
            timeout,
        )
        .await
    }

    /// Approve portal + call `depositEncrypted` on L1 with properly ECIES-encrypted payload.
    ///
    /// Performs ECIES encryption client-side (matching what a real depositor would do):
    /// 1. Read the sequencer's encryption key from the portal
    /// 2. Generate an ephemeral key pair
    /// 3. ECDH → HKDF → AES-256-GCM encrypt (to, memo)
    /// 4. Call `depositEncrypted` on the portal
    /// 5. Wait for the zone to mint tokens to the decrypted recipient
    pub(crate) async fn deposit_encrypted(
        &mut self,
        amount: u128,
        recipient: Address,
        memo: B256,
        timeout: Duration,
        zone: &ZoneTestNode,
    ) -> eyre::Result<U256> {
        use tempo_contracts::precompiles::ITIP20;
        use tempo_precompiles::PATH_USD_ADDRESS;
        use zone::{
            abi::{EncryptedDepositPayload, ZONE_TOKEN_ADDRESS, ZonePortal},
            precompiles::ecies,
        };

        let portal_address = self.portal_address;

        // Approve portal if needed
        if !self.l1_portal_approved {
            ITIP20::new(PATH_USD_ADDRESS, &self.l1_provider)
                .approve(portal_address, U256::MAX)
                .send()
                .await?
                .get_receipt()
                .await?;
            self.l1_portal_approved = true;
        }

        // Read sequencer encryption key and its index from portal
        let portal = ZonePortal::new(portal_address, &self.l1_provider);
        let key_result = portal.sequencerEncryptionKey().call().await?;
        let key_count = portal.encryptionKeyCount().call().await?;
        eyre::ensure!(
            key_count > U256::ZERO,
            "no encryption key registered on portal"
        );
        let key_index = key_count - U256::from(1);

        // ECIES encrypt (to, memo) to sequencer's public key
        let enc = ecies::encrypt_deposit(
            &key_result.x,
            key_result.yParity,
            recipient,
            memo,
            portal_address,
            key_index,
        )
        .ok_or_else(|| eyre::eyre!("ECIES encryption failed"))?;

        // Snapshot balance before deposit
        let balance_before = zone.balance_of(ZONE_TOKEN_ADDRESS, recipient).await?;

        // Call depositEncrypted on portal
        let receipt = portal
            .depositEncrypted(
                PATH_USD_ADDRESS,
                amount,
                key_index,
                EncryptedDepositPayload {
                    ephemeralPubkeyX: enc.eph_pub_x,
                    ephemeralPubkeyYParity: enc.eph_pub_y_parity,
                    ciphertext: enc.ciphertext.into(),
                    nonce: alloy_primitives::FixedBytes(enc.nonce),
                    tag: alloy_primitives::FixedBytes(enc.tag),
                },
            )
            .send()
            .await?
            .get_receipt()
            .await?;
        eyre::ensure!(receipt.status(), "L1 depositEncrypted tx failed");

        // Wait for the zone to process the encrypted deposit and mint to recipient
        zone.wait_for_balance(
            ZONE_TOKEN_ADDRESS,
            recipient,
            balance_before + U256::from(amount),
            timeout,
        )
        .await
    }

    /// Approve the ZoneOutbox, then request a withdrawal on L2.
    ///
    /// Skips approval if already approved in this session.
    pub(crate) async fn withdraw(&mut self, amount: u128) -> eyre::Result<()> {
        self.withdraw_with(WithdrawalArgs::new(amount)).await
    }

    /// Approve the ZoneOutbox, then request a withdrawal on L2 with custom args.
    ///
    /// Skips approval if already approved in this session.
    /// Uses the default zone token (pathUSD / `ZONE_TOKEN_ADDRESS`).
    pub(crate) async fn withdraw_with(&mut self, args: WithdrawalArgs) -> eyre::Result<()> {
        use zone::abi::ZONE_TOKEN_ADDRESS;
        self.withdraw_token_with(ZONE_TOKEN_ADDRESS, args).await
    }

    /// Approve the ZoneOutbox for a specific token, then request a withdrawal on L2.
    pub(crate) async fn withdraw_token(
        &mut self,
        token: Address,
        amount: u128,
    ) -> eyre::Result<()> {
        self.withdraw_token_with(token, WithdrawalArgs::new(amount))
            .await
    }

    /// Approve the ZoneOutbox for a specific token, then request a withdrawal on L2 with custom args.
    pub(crate) async fn withdraw_token_with(
        &mut self,
        token: Address,
        args: WithdrawalArgs,
    ) -> eyre::Result<()> {
        use tempo_contracts::precompiles::ITIP20;
        use zone::abi::{ZONE_OUTBOX_ADDRESS, ZoneOutbox};

        // Approve outbox for this token
        ITIP20::new(token, &self.l2_provider)
            .approve(ZONE_OUTBOX_ADDRESS, U256::MAX)
            .send()
            .await?
            .get_receipt()
            .await?;

        let to = args.to.unwrap_or(self.address);
        let fallback_recipient = args.fallback_recipient.unwrap_or(self.address);

        let outbox = ZoneOutbox::new(ZONE_OUTBOX_ADDRESS, &self.l2_provider);
        let receipt = outbox
            .requestWithdrawal(
                token,
                to,
                args.amount,
                args.memo,
                args.gas_limit,
                fallback_recipient,
                args.data,
            )
            .send()
            .await?
            .get_receipt()
            .await?;
        eyre::ensure!(receipt.status(), "L2 withdrawal request failed");

        Ok(())
    }
}

/// Spawn the zone sequencer background tasks (batch submitter + withdrawal processor).
pub(crate) async fn spawn_sequencer(
    l1: &L1TestNode,
    zone: &ZoneTestNode,
    portal_address: Address,
    sequencer_signer: alloy_signer_local::PrivateKeySigner,
) -> zone::ZoneSequencerHandle {
    use zone::abi::{TEMPO_STATE_ADDRESS, ZONE_INBOX_ADDRESS, ZONE_OUTBOX_ADDRESS};

    let config = zone::ZoneSequencerConfig {
        portal_address,
        l1_rpc_url: l1.http_url().to_string(),
        withdrawal_poll_interval: Duration::from_millis(500),
        outbox_address: ZONE_OUTBOX_ADDRESS,
        inbox_address: ZONE_INBOX_ADDRESS,
        tempo_state_address: TEMPO_STATE_ADDRESS,
        zone_rpc_url: zone.http_url().to_string(),
        zone_poll_interval: Duration::from_millis(500),
        batch_interval: Duration::from_millis(500),
    };

    zone::spawn_zone_sequencer(config, sequencer_signer).await
}

/// Start a local zone node with an L1Fixture already seeded for `seed_blocks` blocks.
pub(crate) async fn start_local_zone_with_fixture(
    seed_blocks: u64,
) -> eyre::Result<(ZoneTestNode, L1Fixture)> {
    let zone = ZoneTestNode::start_local().await?;
    let fixture = L1Fixture::new();
    fixture.seed_l1_cache(zone.l1_state_cache(), Address::ZERO, seed_blocks);
    Ok((zone, fixture))
}

/// Seed an existing L1Fixture's cache into a zone node's L1 state cache.
///
/// Use when multiple zones share the same fixture timeline — call once per zone.
pub(crate) fn seed_fixture_for_zone(fixture: &L1Fixture, zone: &ZoneTestNode, seed_blocks: u64) {
    fixture.seed_l1_cache(zone.l1_state_cache(), Address::ZERO, seed_blocks);
}

// ============ Private RPC Test Utilities ============

/// Build a hex-encoded authorization token for the private zone RPC.
///
/// Signs the token with the given signer and returns the hex string (no `0x` prefix)
/// suitable for the `X-Authorization-Token` header.
fn build_auth_token(
    signer: &alloy_signer_local::PrivateKeySigner,
    zone_id: u64,
    chain_id: u64,
    portal: Address,
) -> String {
    use alloy_signer::SignerSync;
    use zone::rpc::auth::build_token_fields;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let expires_at = now + 600;

    let (fields, digest) = build_token_fields(zone_id, chain_id, portal, now, expires_at);
    let sig = signer.sign_hash_sync(&digest).expect("signing failed");

    let mut blob = Vec::with_capacity(65 + fields.len());
    blob.extend_from_slice(&sig.r().to_be_bytes::<32>());
    blob.extend_from_slice(&sig.s().to_be_bytes::<32>());
    blob.push(sig.v() as u8);
    blob.extend_from_slice(&fields);

    alloy_primitives::hex::encode(&blob)
}

/// Send a JSON-RPC request to the private zone RPC and return the parsed response.
///
/// Returns the full JSON response body (including `jsonrpc`, `id`, `result`/`error`).
async fn private_rpc_call(
    url: &url::Url,
    method: &str,
    params: serde_json::Value,
    auth_token: &str,
) -> eyre::Result<serde_json::Value> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
        "id": 1
    });

    let resp = reqwest::Client::new()
        .post(url.as_str())
        .header("x-authorization-token", auth_token)
        .json(&body)
        .send()
        .await?;

    let status = resp.status();
    let text = resp.text().await?;

    if !status.is_success() && text.is_empty() {
        eyre::bail!("HTTP {status}");
    }

    Ok(serde_json::from_str(&text)?)
}

/// Send a JSON-RPC request to the private zone RPC and return the HTTP status + body.
///
/// Useful for testing authentication failures (401/403).
async fn private_rpc_call_raw(
    url: &url::Url,
    method: &str,
    params: serde_json::Value,
    auth_token: &str,
) -> eyre::Result<(reqwest::StatusCode, String)> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
        "id": 1
    });

    let resp = reqwest::Client::new()
        .post(url.as_str())
        .header("x-authorization-token", auth_token)
        .json(&body)
        .send()
        .await?;

    let status = resp.status();
    let text = resp.text().await?;
    Ok((status, text))
}

/// Send a JSON-RPC request WITHOUT any auth header.
async fn private_rpc_call_no_auth(
    url: &url::Url,
    method: &str,
    params: serde_json::Value,
) -> eyre::Result<(reqwest::StatusCode, String)> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
        "id": 1
    });

    let resp = reqwest::Client::new()
        .post(url.as_str())
        .json(&body)
        .send()
        .await?;

    let status = resp.status();
    let text = resp.text().await?;
    Ok((status, text))
}

/// Context for private RPC e2e tests.
///
/// Wraps a zone node with a running private RPC server in front, providing
/// helpers for authenticated and unauthenticated request testing.
pub(crate) struct PrivateRpcTestCtx {
    /// The underlying zone test node.
    pub zone: ZoneTestNode,
    /// URL of the private RPC server (not the zone's direct HTTP endpoint).
    pub private_rpc_url: url::Url,
    /// The sequencer signer (gets full access on the private RPC).
    pub sequencer_signer: alloy_signer_local::PrivateKeySigner,
    /// Private RPC server configuration.
    pub config: zone::rpc::PrivateRpcConfig,
    /// L1 fixture for injecting deposits.
    pub fixture: L1Fixture,
}

impl PrivateRpcTestCtx {
    /// Build an auth token for the sequencer.
    pub(crate) fn sequencer_token(&self) -> String {
        build_auth_token(
            &self.sequencer_signer,
            self.config.zone_id,
            self.config.chain_id,
            self.config.zone_portal,
        )
    }

    /// Build an auth token for a regular (non-sequencer) user.
    pub(crate) fn user_token(&self, signer: &alloy_signer_local::PrivateKeySigner) -> String {
        build_auth_token(
            signer,
            self.config.zone_id,
            self.config.chain_id,
            self.config.zone_portal,
        )
    }

    /// Send an authenticated JSON-RPC call to the private RPC server.
    pub(crate) async fn call(
        &self,
        method: &str,
        params: serde_json::Value,
        auth_token: &str,
    ) -> eyre::Result<serde_json::Value> {
        private_rpc_call(&self.private_rpc_url, method, params, auth_token).await
    }

    /// Send a JSON-RPC call authenticated as the sequencer.
    pub(crate) async fn call_as_sequencer(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> eyre::Result<serde_json::Value> {
        let token = self.sequencer_token();
        self.call(method, params, &token).await
    }

    /// Send a JSON-RPC call authenticated as a regular user.
    pub(crate) async fn call_as_user(
        &self,
        method: &str,
        params: serde_json::Value,
        signer: &alloy_signer_local::PrivateKeySigner,
    ) -> eyre::Result<serde_json::Value> {
        let token = self.user_token(signer);
        self.call(method, params, &token).await
    }

    /// Send a JSON-RPC call with a raw auth token string, returning HTTP status + body.
    pub(crate) async fn call_raw(
        &self,
        method: &str,
        params: serde_json::Value,
        auth_token: &str,
    ) -> eyre::Result<(reqwest::StatusCode, String)> {
        private_rpc_call_raw(&self.private_rpc_url, method, params, auth_token).await
    }

    /// Send a JSON-RPC call with no auth header, returning HTTP status + body.
    pub(crate) async fn call_no_auth(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> eyre::Result<(reqwest::StatusCode, String)> {
        private_rpc_call_no_auth(&self.private_rpc_url, method, params).await
    }

    /// Build an auth token with custom zone_id, chain_id, and portal (for negative testing).
    pub(crate) fn build_bad_token(
        &self,
        signer: &alloy_signer_local::PrivateKeySigner,
        zone_id: u64,
        chain_id: u64,
        portal: Address,
    ) -> String {
        build_auth_token(signer, zone_id, chain_id, portal)
    }

    /// Inject an empty L1 block and wait for it to be processed.
    pub(crate) async fn inject_empty_block(&mut self) -> eyre::Result<()> {
        let dq = self.zone.deposit_queue().clone();
        self.fixture.inject_empty_block(&dq);
        self.zone
            .wait_for_tempo_block_number(1, DEFAULT_TIMEOUT)
            .await?;
        Ok(())
    }

    /// Inject a deposit and wait for the balance to appear.
    pub(crate) async fn inject_deposit(
        &mut self,
        token: Address,
        depositor: Address,
        recipient: Address,
        amount: u128,
    ) -> eyre::Result<()> {
        let deposit = self
            .fixture
            .make_deposit(token, depositor, recipient, amount);
        let dq = self.zone.deposit_queue().clone();
        self.fixture.inject_deposits(&dq, vec![deposit]);
        self.zone
            .wait_for_balance(token, recipient, U256::from(amount), DEFAULT_TIMEOUT)
            .await?;
        Ok(())
    }

    /// Query `eth_getBalance` via the private RPC as a specific user.
    pub(crate) async fn get_balance_as_user(
        &self,
        address: Address,
        signer: &alloy_signer_local::PrivateKeySigner,
    ) -> eyre::Result<serde_json::Value> {
        self.call_as_user(
            "eth_getBalance",
            serde_json::json!([format!("{address:#x}"), "latest"]),
            signer,
        )
        .await
    }

    /// Query `eth_getBalance` via the private RPC as the sequencer.
    pub(crate) async fn get_balance_as_sequencer(
        &self,
        address: Address,
    ) -> eyre::Result<serde_json::Value> {
        self.call_as_sequencer(
            "eth_getBalance",
            serde_json::json!([format!("{address:#x}"), "latest"]),
        )
        .await
    }

    /// Query `eth_getTransactionCount` via the private RPC as a specific user.
    pub(crate) async fn get_tx_count_as_user(
        &self,
        address: Address,
        signer: &alloy_signer_local::PrivateKeySigner,
    ) -> eyre::Result<serde_json::Value> {
        self.call_as_user(
            "eth_getTransactionCount",
            serde_json::json!([format!("{address:#x}"), "latest"]),
            signer,
        )
        .await
    }
}

/// Start a zone node with a private RPC server for testing.
///
/// Returns a context with:
/// - A running zone node with L1 state cache seeded
/// - A private RPC server on a random port
/// - Sequencer credentials for testing access control
pub(crate) async fn start_zone_with_private_rpc() -> eyre::Result<PrivateRpcTestCtx> {
    use alloy_provider::Provider;

    let zone = ZoneTestNode::start_local().await?;
    let fixture = L1Fixture::new();

    fixture.seed_l1_cache(zone.l1_state_cache(), Address::ZERO, 20);

    let chain_id: alloy_primitives::U64 = zone
        .provider()
        .raw_request("eth_chainId".into(), ())
        .await?;
    let chain_id = chain_id.to::<u64>();

    let sequencer_signer = alloy_signer_local::PrivateKeySigner::random();
    let sequencer_address = sequencer_signer.address();

    let config = zone::rpc::PrivateRpcConfig {
        listen_addr: ([127, 0, 0, 1], 0).into(),
        zone_id: 0,
        chain_id,
        zone_portal: Address::ZERO,
        sequencer: sequencer_address,
    };

    let local_addr = zone::rpc::start_private_rpc(config.clone(), zone.rpc_api()).await?;
    let private_rpc_url: url::Url = format!("http://{local_addr}").parse()?;

    Ok(PrivateRpcTestCtx {
        zone,
        private_rpc_url,
        sequencer_signer,
        config,
        fixture,
    })
}

/// A synthetic L1 block produced by [`L1Fixture`].
///
/// Clonable so the same block can be enqueued into multiple zone deposit queues,
/// simulating multiple zones observing the same L1 block.
#[derive(Clone)]
pub(crate) struct FixtureBlock {
    /// The L1 block header. Use `header.inner.number` to read the block number.
    pub header: TempoHeader,
}

/// Builder for creating realistic L1 block headers and deposits for injection
/// into a [`ZoneTestNode`]'s deposit queue.
///
/// Maintains monotonic block numbers and timestamps, and chains parent hashes
/// to mirror what the real L1Subscriber would produce.
pub(crate) struct L1Fixture {
    next_block_number: u64,
    next_timestamp: u64,
    last_hash: B256,
}

impl L1Fixture {
    pub(crate) fn new() -> Self {
        // TempoState stores tempoBlockHash = keccak256(rlp(default TempoHeader)),
        // so the first injected L1 block must have parent_hash matching this.
        let genesis_header = TempoHeader::default();
        let mut rlp_buf = Vec::new();
        genesis_header.encode(&mut rlp_buf);
        let genesis_hash = keccak256(&rlp_buf);

        Self {
            next_block_number: 1,
            next_timestamp: 1_000_000,
            last_hash: genesis_hash,
        }
    }

    /// Pre-populate the L1 state cache with values that `advanceTempo` will read
    /// via the TempoStateReader precompile.
    ///
    /// Without a real L1, the precompile would fail with a hard error on cache miss.
    /// This seeds the cache so that `readStorageAt(portal, slot, blockNumber)` succeeds
    /// for each block we plan to inject.
    pub(crate) fn seed_l1_cache(
        &self,
        cache: &SharedL1StateCache,
        portal_address: Address,
        num_blocks: u64,
    ) {
        let mut cache = cache.write();
        let deposit_queue_hash_slot = B256::with_last_byte(4);

        for block in 0..=num_blocks {
            // Sequencer slot (0) — not actually read if msg.sender == address(0),
            // but seed it to be safe.
            cache.set(portal_address, B256::ZERO, block, B256::ZERO);
            // Deposit queue hash slot (4) — read by ZoneInbox after finalizeTempo.
            // The initial value is B256::ZERO (empty queue).
            cache.set(portal_address, deposit_queue_hash_slot, block, B256::ZERO);
        }
    }

    /// Build a [`TempoHeader`] for the next L1 block.
    fn next_header(&mut self) -> TempoHeader {
        let number = self.next_block_number;
        let timestamp = self.next_timestamp;
        let parent_hash = self.last_hash;

        let header = TempoHeader {
            inner: Header {
                number,
                timestamp,
                parent_hash,
                ..Default::default()
            },
            ..Default::default()
        };

        // Advance state: TempoState stores keccak256(rlp(header)) as tempoBlockHash,
        // so the next block's parent_hash must match this value.
        let mut rlp_buf = Vec::new();
        header.encode(&mut rlp_buf);
        self.last_hash = keccak256(&rlp_buf);
        self.next_block_number += 1;
        self.next_timestamp += 1; // 1s per L1 block

        header
    }

    /// Build the next L1 block without injecting it into any queue.
    ///
    /// Use with [`enqueue`](Self::enqueue) to broadcast the same block
    /// to multiple zone deposit queues.
    pub(crate) fn next_block(&mut self) -> FixtureBlock {
        let header = self.next_header();
        FixtureBlock { header }
    }

    /// Enqueue a pre-built block into a deposit queue with the given deposits.
    pub(crate) fn enqueue(
        &self,
        block: &FixtureBlock,
        queue: &DepositQueue,
        deposits: Vec<Deposit>,
    ) {
        let l1_deposits = deposits.into_iter().map(L1Deposit::Regular).collect();
        let events = L1PortalEvents::from_deposits(l1_deposits);
        queue.enqueue(block.header.clone(), events);
    }

    /// Create a [`Deposit`] tied to a specific L1 block number.
    pub(crate) fn make_deposit_for_block(
        l1_block_number: u64,
        token: Address,
        sender: Address,
        to: Address,
        amount: u128,
    ) -> Deposit {
        Deposit {
            l1_block_number,
            token,
            sender,
            to,
            amount,
            fee: 0,
            memo: B256::ZERO,
            queue_hash: B256::ZERO,
        }
    }

    /// Inject an empty L1 block (no deposits) into the queue.
    pub(crate) fn inject_empty_block(&mut self, queue: &DepositQueue) {
        let header = self.next_header();
        queue.enqueue(header, L1PortalEvents::default());
    }

    /// Inject `n` empty L1 blocks (no deposits) into the queue.
    pub(crate) fn inject_empty_blocks(&mut self, queue: &DepositQueue, n: u64) {
        for _ in 0..n {
            self.inject_empty_block(queue);
        }
    }

    /// Inject an L1 block with the given deposits into the queue.
    pub(crate) fn inject_deposits(&mut self, queue: &DepositQueue, deposits: Vec<Deposit>) {
        let header = self.next_header();
        let l1_deposits = deposits.into_iter().map(L1Deposit::Regular).collect();
        let events = L1PortalEvents::from_deposits(l1_deposits);
        queue.enqueue(header, events);
    }

    /// Inject an L1 block with mixed regular and encrypted deposits.
    #[allow(dead_code)]
    pub(crate) fn inject_l1_deposits(&mut self, queue: &DepositQueue, deposits: Vec<L1Deposit>) {
        let header = self.next_header();
        let events = L1PortalEvents::from_deposits(deposits);
        queue.enqueue(header, events);
    }

    /// Create an [`EncryptedDeposit`] for testing with dummy ECIES parameters.
    #[allow(dead_code)]
    pub(crate) fn make_encrypted_deposit(
        &self,
        token: Address,
        sender: Address,
        amount: u128,
    ) -> EncryptedDeposit {
        EncryptedDeposit {
            l1_block_number: self.next_block_number,
            token,
            sender,
            amount,
            fee: 0,
            key_index: alloy_primitives::U256::ZERO,
            ephemeral_pubkey_x: B256::ZERO,
            ephemeral_pubkey_y_parity: 0x02,
            ciphertext: vec![0u8; 64], // ENCRYPTED_PAYLOAD_PLAINTEXT_SIZE = 64
            nonce: [0u8; 12],
            tag: [0u8; 16],
            queue_hash: B256::ZERO,
        }
    }

    /// Create a [`Deposit`] for testing.
    pub(crate) fn make_deposit(
        &self,
        token: Address,
        sender: Address,
        to: Address,
        amount: u128,
    ) -> Deposit {
        Deposit {
            l1_block_number: self.next_block_number,
            token,
            sender,
            to,
            amount,
            fee: 0,
            memo: B256::ZERO,
            queue_hash: B256::ZERO,
        }
    }

    /// Create an [`EncryptedDeposit`] with proper ECIES encryption against the
    /// sequencer's real public key.
    ///
    /// Uses a deterministic ephemeral key for reproducibility.
    #[allow(clippy::too_many_arguments)]
    #[allow(dead_code)]
    pub(crate) fn make_real_encrypted_deposit(
        &self,
        sequencer_pub: &k256::AffinePoint,
        portal_address: Address,
        key_index: alloy_primitives::U256,
        token: Address,
        sender: Address,
        recipient: Address,
        amount: u128,
        memo: B256,
    ) -> EncryptedDeposit {
        use k256::{ProjectivePoint, Scalar, elliptic_curve::sec1::ToEncodedPoint};
        use sha2::{Digest, Sha256};
        use zone::precompiles::ecies::{
            build_plaintext, compressed_x_and_parity, encrypt_plaintext, hkdf_sha256,
        };

        // Deterministic ephemeral key for reproducibility
        let eph_bytes: [u8; 32] = Sha256::digest(b"test-ephemeral-key-for-e2e").into();
        let eph_key = k256::SecretKey::from_slice(&eph_bytes).expect("valid ephemeral key");
        let eph_scalar: Scalar = *eph_key.to_nonzero_scalar();
        let eph_pub = k256::AffinePoint::from(ProjectivePoint::GENERATOR * eph_scalar);
        let (eph_pub_x, eph_pub_y_parity) = compressed_x_and_parity(&eph_pub);

        // ECDH: shared = eph_scalar * sequencer_pub
        let shared_proj = ProjectivePoint::from(*sequencer_pub) * eph_scalar;
        let shared_affine = k256::AffinePoint::from(shared_proj);
        let ss_enc = shared_affine.to_encoded_point(true);
        let shared_secret_x: [u8; 32] = ss_enc.x().unwrap().as_slice().try_into().unwrap();

        // HKDF-SHA256 key derivation (matching ecies.rs)
        let mut info = Vec::with_capacity(84);
        info.extend_from_slice(portal_address.as_slice());
        info.extend_from_slice(&key_index.to_be_bytes::<32>());
        info.extend_from_slice(&eph_pub_x.0);
        let aes_key = hkdf_sha256(&shared_secret_x, b"ecies-aes-key", &info);

        // Build and encrypt plaintext (deterministic zero nonce)
        let plaintext = build_plaintext(&recipient, &memo);
        let (ciphertext, nonce, tag) = encrypt_plaintext(&aes_key, &plaintext);

        EncryptedDeposit {
            l1_block_number: self.next_block_number,
            token,
            sender,
            amount,
            fee: 0,
            key_index,
            ephemeral_pubkey_x: eph_pub_x,
            ephemeral_pubkey_y_parity: eph_pub_y_parity,
            ciphertext,
            nonce,
            tag,
            queue_hash: B256::ZERO,
        }
    }
}
