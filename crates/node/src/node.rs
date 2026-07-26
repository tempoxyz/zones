//! Tempo Zone Node configuration.
//!
//! This is a lightweight L2 node built on reth's node builder infrastructure.
//! It reuses Tempo's EVM, primitives, and pool, but with noop consensus/network/payload.

use crate::{
    ZoneEngine,
    replication::{AttestationContext, broadcast_persisted_blocks, run_block_sync},
    rpc::{ZoneRpc, ZoneRpcApi, rpc_connection_config, start_private_rpc},
    settlement_attestation::collect_leader_settlements,
    tx_forwarding::{forward_new_transactions, insert_forwarded_transactions, route_p2p_events},
};
use alloy_primitives::Address;
use alloy_provider::Provider as _;
use alloy_signer_local::PrivateKeySigner;
use k256::SecretKey;
use reth_chainspec::EthChainSpec;
use reth_eth_wire_types::primitives::BasicNetworkPrimitives;
use reth_node_api::{
    AddOnsContext, ConsensusEngineHandle, FullNodeComponents, FullNodeTypes, NodeAddOns, NodeTypes,
    PayloadAttributesBuilder, PayloadTypes,
};
use reth_node_builder::{
    BuilderContext, DebugNode, Node, NodeAdapter,
    components::{
        BasicPayloadServiceBuilder, ComponentsBuilder, ExecutorBuilder, NoopConsensusBuilder,
        NoopNetworkBuilder, PoolBuilder, spawn_maintenance_tasks,
    },
    rpc::{
        BasicEngineValidatorBuilder, EngineValidatorAddOn, EthApiBuilder, NoopEngineApiBuilder,
        PayloadValidatorBuilder, RethRpcAddOns, RpcAddOns,
    },
};
use reth_primitives_traits::SealedHeader;
use reth_provider::ChainSpecProvider;
use reth_rpc_builder::Identity;
use reth_rpc_eth_api::EthApiTypes;
use reth_storage_api::{
    BlockNumReader, EmptyBodyStorage, HeaderProvider, StateProvider, StateProviderFactory,
};
use reth_transaction_pool::{
    Pool, PoolTransaction, TransactionPool as _, TransactionValidationTaskExecutor,
    blobstore::InMemoryBlobStore, error::InvalidPoolTransactionError,
};
use std::{num::NonZeroU32, sync::Arc, time::Duration};
use tempo_alloy::TempoNetwork;
use tempo_chainspec::spec::{DEV, TempoChainSpec, chainspec_from_chain_id};
use tempo_evm::TempoInvalidTransaction;
use tempo_node::{
    DEFAULT_AA_VALID_AFTER_MAX_SECS, engine::TempoEngineValidator, rpc::TempoEthApiBuilder,
};
use tempo_precompiles::tip20::TIP20Token;
use tempo_primitives::{
    self as primitives, TempoHeader, TempoPrimitives, TempoTxEnvelope, TempoTxType,
};
use tempo_transaction_pool::{
    AA2dPool, AA2dPoolConfig, TempoTransactionPool,
    amm::AmmLiquidityCache,
    ordering::TempoTipOrdering,
    transaction::{TempoPoolTransactionError, TempoPooledTransaction},
    validator::{DEFAULT_MAX_TEMPO_AUTHORIZATIONS, TempoTransactionValidator},
};
use tempo_zone_contracts::{
    TEMPO_STATE_ADDRESS, ZONE_INBOX_ADDRESS, ZONE_OUTBOX_ADDRESS, ZonePortal,
};
use tracing::{debug, info, warn};
use zone_chainspec::ZoneChainSpec;
use zone_evm::ZoneEvmConfig;
use zone_l1::{
    DepositQueue, L1BlockTracker, L1Subscriber, L1SubscriberConfig, TempoStateExt,
    state::{EnabledTokenRegistry, L1StateCache, L1StateProvider, L1StateProviderConfig},
};
use zone_p2p::{P2pConfig, P2pNetworkId, Role, spawn_p2p};
use zone_payload::{
    DEFAULT_WITHDRAWAL_BATCH_INTERVAL_BLOCKS, WithdrawalRevealEncryptor, ZonePayloadAttributes,
    ZonePayloadFactory, ZonePayloadTypes,
};
use zone_sequencer::{
    AttestationStore, BatchAnchorConfig, WithdrawalBatchLimits, ZoneSequencerConfig,
    attestation::AttestationDomain, spawn_zone_sequencer,
};

/// Returns a known Tempo chain spec for an L1 chain ID.
///
/// Tempo Anvil uses chain ID 31337 and the same hardfork schedule as Tempo DEV (1337).
///
/// Additional dev-schedule L1 chain IDs (devnets that activate all Tempo
/// hardforks at genesis) can be allowed via the `ZONE_L1_DEV_CHAIN_IDS`
/// environment variable as a comma-separated list.
fn tempo_chain_spec_for_l1(chain_id: u64) -> Option<Arc<TempoChainSpec>> {
    chainspec_from_chain_id(chain_id).or_else(|| match chain_id {
        1337 | 31337 => Some(DEV.clone()),
        _ => std::env::var("ZONE_L1_DEV_CHAIN_IDS")
            .ok()?
            .split(',')
            .any(|id| id.trim().parse() == Ok(chain_id))
            .then(|| DEV.clone()),
    })
}

/// Network primitives for Zone Nodes
type ZoneNetworkPrimitives = BasicNetworkPrimitives<TempoPrimitives, TempoTxEnvelope>;

/// Sequencer-side sender reveal encryptor used while building
/// `finalizeWithdrawalBatch` system transactions.
///
/// The encrypted sender payload is hashed into withdrawal data, so ECIES must
/// not use fresh randomness here. This implementation derives reproducible
/// encryption material from the sequencer encryption key, zone id, reveal key,
/// sender, and withdrawal transaction hash, which keeps identical withdrawal
/// batches byte-for-byte stable across sequencers.
struct SequencerWithdrawalRevealEncryptor {
    encryption_key: Arc<SecretKey>,
    zone_id: u32,
}

impl SequencerWithdrawalRevealEncryptor {
    fn new(encryption_key: SecretKey, zone_id: u32) -> Self {
        Self {
            encryption_key: Arc::new(encryption_key),
            zone_id,
        }
    }
}

impl std::fmt::Debug for SequencerWithdrawalRevealEncryptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SequencerWithdrawalRevealEncryptor")
            .field("zone_id", &self.zone_id)
            .finish_non_exhaustive()
    }
}

impl WithdrawalRevealEncryptor for SequencerWithdrawalRevealEncryptor {
    fn encrypt_sender(
        &self,
        reveal_to: &[u8],
        sender: Address,
        tx_hash: alloy_primitives::B256,
    ) -> Option<Vec<u8>> {
        zone_precompiles::ecies::encrypt_authenticated_withdrawal_deterministic(
            &self.encryption_key,
            self.zone_id,
            reveal_to,
            sender,
            tx_hash,
        )
    }
}

/// Configuration for the sequencer background tasks
#[derive(Debug, Clone)]
pub struct ZoneSequencerAddOnsConfig {
    /// Shared sequencer signer used for block production and encryption.
    pub sequencer_signer: PrivateKeySigner,
    /// Individual manifest-node signer used for L1 settlement transactions.
    pub l1_transaction_signer: Option<PrivateKeySigner>,
    /// Zone ID for chain ID validation.
    pub zone_id: u32,
    /// How often the zone monitor polls for new L2 blocks.
    pub zone_poll_interval: Duration,
    /// Number of zone blocks between withdrawal batch boundaries / L1 submissions.
    pub batch_interval_blocks: u64,
    /// EIP-2935 history and safety-margin limits used by the batch submitter.
    pub batch_anchor_config: BatchAnchorConfig,
    /// How often the withdrawal processor polls the L1 queue.
    pub withdrawal_poll_interval: Duration,
    /// Gas and concurrency limits for withdrawal processing transactions.
    pub withdrawal_batch_limits: WithdrawalBatchLimits,
}

/// Configuration for the Zone private RPC server extension.
#[derive(Debug, Clone, Default)]
pub struct ZonePrivateRpcConfig {
    /// Port for RPC traffic.
    pub private_rpc_port: u16,
    /// Zone ID for chain ID validation and private RPC auth.
    pub zone_id: u32,
    /// Max duration for private RPC auth.
    pub max_auth_token_validity: Duration,
}

/// Tempo Zone node type configuration.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ZoneNode {
    /// Queue of L1 deposit messages to be included in the next zone block.
    deposit_queue: DepositQueue,
    /// Configuration for the L1 event subscriber (RPC endpoint, retries, etc.).
    l1_config: L1SubscriberConfig,
    /// Configuration for the L1 state provider (contract addresses, query parameters).
    l1_state_provider_config: L1StateProviderConfig,
    /// Shared L1 state cache (enabled tokens, zone metadata, etc.).
    l1_state_cache: L1StateCache,
    /// Shared registry of tokens enabled for this zone.
    enabled_tokens: EnabledTokenRegistry,
    /// L1 anchors independently observed and applied by the subscriber.
    l1_block_tracker: L1BlockTracker,
    /// Address of the L1 deposit portal contract.
    portal_address: Address,
    /// Number of zone blocks between withdrawal batch boundaries.
    withdrawal_batch_interval_blocks: u64,
    /// Encrypts authenticated-withdrawal sender reveal data during payload construction.
    withdrawal_reveal_encryptor: Option<Arc<dyn WithdrawalRevealEncryptor>>,
    /// Private RPC config.
    private_rpc_config: ZonePrivateRpcConfig,
    /// Optional sequencer config. When set, sequencer tasks are spawned.
    sequencer_config: Option<ZoneSequencerAddOnsConfig>,
    /// Optional static Zone P2P networking config.
    p2p_config: Option<P2pConfig>,
    /// Whether a consumer outside this builder drains the deposit queue.
    external_deposit_consumer: bool,
}

impl ZoneNode {
    // Creates a new ZoneNode
    pub fn new(
        l1_rpc_url: String,
        portal_address: Address,
        genesis_tempo_block_number: Option<u64>,
        l1_fetch_concurrency: usize,
        retry_connection_interval: Duration,
    ) -> Self {
        let deposit_queue = DepositQueue::default();

        let l1_state_cache = L1StateCache::new();
        let enabled_tokens = EnabledTokenRegistry::default();
        let l1_block_tracker = L1BlockTracker::default();
        let l1_config = L1SubscriberConfig {
            l1_rpc_url: l1_rpc_url.clone(),
            portal_address,
            genesis_tempo_block_number,
            enabled_tokens: enabled_tokens.clone(),
            l1_state_cache: l1_state_cache.clone(),
            block_tracker: l1_block_tracker.clone(),
            l1_fetch_concurrency,
            retry_connection_interval,
            retain_observations: false,
        };

        let l1_state_provider_config = L1StateProviderConfig {
            l1_rpc_url,
            portal_address,
            retry_connection_interval,
            ..Default::default()
        };

        Self {
            deposit_queue,
            l1_config,
            l1_state_provider_config,
            l1_state_cache,
            enabled_tokens,
            l1_block_tracker,
            portal_address,
            withdrawal_batch_interval_blocks: DEFAULT_WITHDRAWAL_BATCH_INTERVAL_BLOCKS,
            withdrawal_reveal_encryptor: None,
            private_rpc_config: ZonePrivateRpcConfig::default(),
            sequencer_config: None,
            p2p_config: None,
            external_deposit_consumer: false,
        }
    }

    /// Set the private RPC configuration.
    pub fn with_private_rpc(mut self, config: ZonePrivateRpcConfig) -> Self {
        self.private_rpc_config = config;
        self
    }

    /// Set the sequencer configuration. When set, batch submission and
    /// withdrawal processing tasks are spawned during node launch.
    pub fn with_sequencer(mut self, config: ZoneSequencerAddOnsConfig) -> Self {
        let encryption_key = SecretKey::from(config.sequencer_signer.credential());
        self.withdrawal_reveal_encryptor = Some(Arc::new(SequencerWithdrawalRevealEncryptor::new(
            encryption_key,
            config.zone_id,
        )));
        self.sequencer_config = Some(config);
        self
    }

    /// Declare that a consumer outside this builder drains [`Self::deposit_queue`].
    ///
    /// Without a sequencer or P2P config the node assumes nothing consumes deposits and
    /// launches a sink-less L1 observer. Callers that drive their own [`crate::ZoneEngine`]
    /// against the shared queue — such as test harnesses — must opt back into retention.
    pub fn with_external_deposit_consumer(mut self) -> Self {
        self.external_deposit_consumer = true;
        self
    }

    /// Enable static Zone P2P networking for this node.
    pub fn with_p2p(mut self, config: P2pConfig) -> Self {
        // Multi-sequencer members gate follower block import on independently observed L1
        // anchors, so observations must survive until their zone block is consumed.
        self.l1_config.retain_observations = true;
        self.p2p_config = Some(config);
        self
    }

    /// Set the encryptor used for authenticated-withdrawal sender reveal data.
    pub fn with_withdrawal_reveal_encryptor(
        mut self,
        encryptor: Arc<dyn WithdrawalRevealEncryptor>,
    ) -> Self {
        self.withdrawal_reveal_encryptor = Some(encryptor);
        self
    }

    /// Set the parent L1 chain ID, avoiding a startup RPC lookup.
    pub fn with_l1_chain_id(mut self, chain_id: u64) -> Self {
        self.l1_state_provider_config.chain_id = Some(chain_id);
        self
    }

    /// Bound L1 state-provider retries for callers that must fail finitely on cache misses.
    pub fn with_l1_state_provider_retry_limits(
        mut self,
        transport_retries: u32,
        sync_attempts: NonZeroU32,
    ) -> Self {
        self.l1_state_provider_config.max_retries = transport_retries;
        self.l1_state_provider_config.max_sync_attempts = Some(sync_attempts);
        self
    }

    /// Set the number of zone blocks between empty withdrawal batch
    /// finalization.
    pub fn with_withdrawal_batch_interval_blocks(mut self, interval_blocks: u64) -> Self {
        self.withdrawal_batch_interval_blocks = interval_blocks.max(1);
        self
    }

    /// Returns the current deposit queue
    pub fn deposit_queue(&self) -> DepositQueue {
        self.deposit_queue.clone()
    }

    /// Returns the current l1 state cache
    pub fn l1_state_cache(&self) -> L1StateCache {
        self.l1_state_cache.clone()
    }

    /// Returns the shared enabled-token registry.
    pub fn enabled_tokens(&self) -> EnabledTokenRegistry {
        self.enabled_tokens.clone()
    }

    /// Returns the L1 block observation tracker.
    pub fn l1_block_tracker(&self) -> L1BlockTracker {
        self.l1_block_tracker.clone()
    }
    /// Returns a [`ComponentsBuilder`] configured for a Zone node.
    pub fn components<N>(
        executor_builder: ZoneExecutorBuilder,
    ) -> ComponentsBuilder<
        N,
        ZonePoolBuilder,
        BasicPayloadServiceBuilder<ZonePayloadFactory>,
        NoopNetworkBuilder<ZoneNetworkPrimitives>,
        ZoneExecutorBuilder,
        NoopConsensusBuilder,
    >
    where
        N: FullNodeTypes<Types = Self>,
    {
        Self::components_with_payload_factory(executor_builder, ZonePayloadFactory::default())
    }

    fn components_with_payload_factory<N>(
        executor_builder: ZoneExecutorBuilder,
        payload_factory: ZonePayloadFactory,
    ) -> ComponentsBuilder<
        N,
        ZonePoolBuilder,
        BasicPayloadServiceBuilder<ZonePayloadFactory>,
        NoopNetworkBuilder<ZoneNetworkPrimitives>,
        ZoneExecutorBuilder,
        NoopConsensusBuilder,
    >
    where
        N: FullNodeTypes<Types = Self>,
    {
        ComponentsBuilder::default()
            .node_types::<N>()
            .pool(ZonePoolBuilder::new(
                executor_builder.enabled_tokens.clone(),
            ))
            .executor(executor_builder)
            .payload(BasicPayloadServiceBuilder::new(payload_factory))
            .network(NoopNetworkBuilder::<ZoneNetworkPrimitives>::default())
            .noop_consensus()
    }
}

impl NodeTypes for ZoneNode {
    type Primitives = TempoPrimitives;
    type ChainSpec = ZoneChainSpec;
    type Storage = EmptyBodyStorage<TempoTxEnvelope, TempoHeader>;
    type Payload = ZonePayloadTypes;
}

/// Addons for Tempo Zone nodes.
pub struct ZoneAddOns<N>
where
    N: FullNodeComponents<Types = ZoneNode, Evm = ZoneEvmConfig>,
    N::Pool: reth_transaction_pool::TransactionPool<Transaction = TempoPooledTransaction>,
{
    inner: RpcAddOns<
        N,
        TempoEthApiBuilder<N>,
        ZoneEngineValidatorBuilder,
        NoopEngineApiBuilder,
        BasicEngineValidatorBuilder<ZoneEngineValidatorBuilder>,
        Identity,
    >,
    /// Queue of L1 deposit messages to be included in the next zone block.
    deposit_queue: DepositQueue,
    /// Configuration for the L1 event subscriber
    l1_config: L1SubscriberConfig,
    /// ZonePortal address on L1.
    portal_address: Address,
    /// Private RPC configuration.
    private_rpc_config: ZonePrivateRpcConfig,
    /// Sequencer configuration.
    sequencer_config: Option<ZoneSequencerAddOnsConfig>,
    /// Static Zone P2P networking configuration.
    p2p_config: Option<P2pConfig>,
    /// Whether a consumer outside this builder drains the deposit queue.
    external_deposit_consumer: bool,
}

impl<N> std::fmt::Debug for ZoneAddOns<N>
where
    N: FullNodeComponents<Types = ZoneNode, Evm = ZoneEvmConfig>,
    N::Pool: reth_transaction_pool::TransactionPool<Transaction = TempoPooledTransaction>,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZoneAddOns").finish_non_exhaustive()
    }
}

impl<N> ZoneAddOns<NodeAdapter<N>>
where
    N: FullNodeTypes<Types = ZoneNode>,
{
    /// Creates a new ZoneAddOns instance.
    pub fn new(
        deposit_queue: DepositQueue,
        l1_config: L1SubscriberConfig,
        portal_address: Address,
        private_rpc_config: ZonePrivateRpcConfig,
        sequencer_config: Option<ZoneSequencerAddOnsConfig>,
        p2p_config: Option<P2pConfig>,
        external_deposit_consumer: bool,
    ) -> Self {
        Self {
            inner: RpcAddOns::new(
                TempoEthApiBuilder::default(),
                ZoneEngineValidatorBuilder,
                NoopEngineApiBuilder::default(),
                BasicEngineValidatorBuilder::default(),
                Identity::default(),
                Default::default(),
            ),
            deposit_queue,
            l1_config,
            portal_address,
            private_rpc_config,
            sequencer_config,
            p2p_config,
            external_deposit_consumer,
        }
    }
}

impl<N> NodeAddOns<N> for ZoneAddOns<N>
where
    N: FullNodeComponents<Types = ZoneNode, Evm = ZoneEvmConfig>,
    N::Pool: reth_transaction_pool::TransactionPool<
            Transaction = tempo_transaction_pool::transaction::TempoPooledTransaction,
        >,
    TempoEthApiBuilder<N>: EthApiBuilder<N, EthApi: EthApiTypes<NetworkTypes = TempoNetwork>>,
{
    type Handle = <RpcAddOns<
        N,
        TempoEthApiBuilder<N>,
        ZoneEngineValidatorBuilder,
        NoopEngineApiBuilder,
        BasicEngineValidatorBuilder<ZoneEngineValidatorBuilder>,
        Identity,
    > as NodeAddOns<N>>::Handle;

    async fn launch_add_ons(mut self, ctx: AddOnsContext<'_, N>) -> eyre::Result<Self::Handle> {
        let tempo_block_number = ctx.node.provider().latest()?.tempo_block_number()?;
        // A fresh zero-genesis node has not imported an L1 header yet, so its on-chain
        // checkpoint is zero even when the configured replay cursor is later. Seed the
        // registry from the newest snapshot represented by either source. On restart, the
        // imported checkpoint naturally takes precedence over the original genesis cursor.
        let token_snapshot_block = self
            .l1_config
            .genesis_tempo_block_number
            .map_or(tempo_block_number, |genesis| {
                genesis.max(tempo_block_number)
            });
        let l1_provider = alloy_provider::ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect_with_config(
                &self.l1_config.l1_rpc_url,
                rpc_connection_config(self.l1_config.retry_connection_interval),
            )
            .await?
            .erased();

        self.resolve_and_seed_tokens(&l1_provider, token_snapshot_block)
            .await?;

        self.spawn_l1_subscriber(&ctx);

        let task_executor = ctx.node.task_executor().clone();
        let attestation_store = self
            .p2p_config
            .as_ref()
            .filter(|config| config.role() == Role::Leader)
            .map(|_| AttestationStore::default());
        if let Some(config) = self.p2p_config.take() {
            let l1_chain_id = l1_provider.get_chain_id().await?;
            let network_id = P2pNetworkId::new(l1_chain_id, self.portal_address);
            let attestation_domain = AttestationDomain {
                l1_chain_id,
                portal_address: self.portal_address,
                zone_id: config.zone_id(),
                sequencer_set_version: config.sequencer_set_version(),
            };
            let anchor_config = self
                .sequencer_config
                .as_ref()
                .map(|config| config.batch_anchor_config)
                .unwrap_or_default();
            let attestation = AttestationContext::new(
                attestation_domain,
                config.block_attestation_signer(),
                config.block_attestation_addresses(),
                attestation_store.clone(),
                l1_provider.clone(),
                anchor_config,
            );
            Self::launch_p2p(
                config,
                network_id,
                attestation,
                &task_executor,
                ctx.node.provider().clone(),
                ctx.node.pool().clone(),
                ctx.beacon_engine_handle.clone(),
                self.l1_config.block_tracker.clone(),
                self.deposit_queue.clone(),
            )?;
        }

        if let Some(ref config) = self.sequencer_config {
            let sequencer_addr = config.sequencer_signer.address();
            let sequencer_key = SecretKey::from(config.sequencer_signer.credential());
            self.spawn_zone_engine(&ctx, sequencer_addr, sequencer_key)?;
        }

        let chain_id = ctx.node.provider().chain_spec().genesis().config.chain_id;
        let handle = self.inner.launch_add_ons(ctx).await?;

        Self::launch_private_rpc(
            self.private_rpc_config,
            &handle,
            self.l1_config.l1_rpc_url.clone(),
            self.l1_config.retry_connection_interval,
            self.l1_config.portal_address,
            chain_id,
        )
        .await?;

        if let Some(config) = self.sequencer_config.take() {
            let sequencer_addr = config.sequencer_signer.address();

            Self::launch_sequencer_tasks(
                config,
                &handle,
                &task_executor,
                self.l1_config.l1_rpc_url,
                self.l1_config.portal_address,
                self.l1_config.retry_connection_interval,
                sequencer_addr,
                chain_id,
                attestation_store,
            )
            .await?;
        }

        Ok(handle)
    }
}

impl<N> ZoneAddOns<N>
where
    N: FullNodeComponents<Types = ZoneNode, Evm = ZoneEvmConfig>,
    N::Pool: reth_transaction_pool::TransactionPool<
            Transaction = tempo_transaction_pool::transaction::TempoPooledTransaction,
        >,
    TempoEthApiBuilder<N>: EthApiBuilder<N, EthApi: EthApiTypes<NetworkTypes = TempoNetwork>>,
{
    fn launch_p2p(
        config: P2pConfig,
        network_id: P2pNetworkId,
        attestation: AttestationContext,
        task_executor: &reth_tasks::TaskExecutor,
        provider: N::Provider,
        pool: N::Pool,
        engine: ConsensusEngineHandle<ZonePayloadTypes>,
        l1_block_tracker: L1BlockTracker,
        deposit_queue: DepositQueue,
    ) -> eyre::Result<()> {
        let local_ed25519_public_key = config.ed25519_public_key();
        let leadership = config.leadership();
        let role = leadership.role_of(&local_ed25519_public_key);
        // Subscribe before starting Commonware (and, importantly, before RPC launch) so a
        // follower cannot admit a transaction in a startup gap.
        let new_transactions = (role == Role::Follower).then(|| pool.new_transactions_listener());
        let handle = spawn_p2p(config, network_id)?;
        let zone_p2p::P2pHandleParts {
            shutdown: shutdown_token,
            mut stopped,
            thread,
            commands,
            events,
        } = handle.into_parts();

        let sync_events = match role {
            Role::Leader => {
                task_executor.spawn_critical_task(
                    "zone-p2p-block-broadcast",
                    broadcast_persisted_blocks(provider.clone(), commands.clone()),
                );
                let (sync_events_tx, sync_events) = tokio::sync::mpsc::channel(128);
                let (transaction_events_tx, transaction_events) = tokio::sync::mpsc::channel(128);
                task_executor.spawn_critical_task(
                    "zone-p2p-event-router",
                    route_p2p_events(events, sync_events_tx, transaction_events_tx),
                );
                task_executor.spawn_critical_task(
                    "zone-p2p-transaction-import",
                    insert_forwarded_transactions(pool, transaction_events),
                );
                sync_events
            }
            Role::Follower => {
                task_executor.spawn_critical_task(
                    "zone-p2p-transaction-forward",
                    forward_new_transactions(
                        pool,
                        new_transactions.expect("follower listener must be initialized"),
                        commands.clone(),
                    ),
                );
                events
            }
        };
        task_executor.spawn_critical_task(
            "zone-p2p-block-sync",
            run_block_sync(
                local_ed25519_public_key,
                leadership,
                provider.clone(),
                engine,
                sync_events,
                commands.clone(),
                l1_block_tracker,
                deposit_queue,
                attestation.clone(),
            ),
        );
        if role == Role::Leader {
            // Only a leader can propose settlement attestations
            task_executor.spawn_critical_task(
                "zone-p2p-settlement-collection",
                collect_leader_settlements(provider, commands, attestation),
            );
        }
        task_executor.spawn_critical_with_graceful_shutdown_signal(
            "zone-p2p",
            |shutdown| async move {
                let unexpected_exit = tokio::select! {
                    guard = shutdown => {
                        let _guard = guard;
                        shutdown_token.cancel();
                        match stopped.await {
                            Ok(Ok(())) => info!(target: "reth::cli", "P2P runtime stopped"),
                            Ok(Err(err)) => tracing::error!(target: "reth::cli", %err, "P2P runtime failed during shutdown"),
                            Err(err) => tracing::error!(target: "reth::cli", %err, "P2P runtime completion channel closed during shutdown"),
                        }
                        None
                    }
                    result = &mut stopped => {
                        Some(match result {
                            Ok(Ok(())) => "P2P runtime stopped unexpectedly".to_string(),
                            Ok(Err(err)) => format!("P2P runtime failed: {err}"),
                            Err(err) => format!("P2P runtime completion channel closed unexpectedly: {err}"),
                        })
                    }
                };

                match tokio::task::spawn_blocking(move || thread.join()).await {
                    Ok(Ok(())) => {}
                    Ok(Err(_)) => panic!("P2P runtime thread panicked"),
                    Err(err) => panic!("Failed joining P2P runtime thread: {err}"),
                }

                if let Some(reason) = unexpected_exit {
                    panic!("{reason}");
                }
            },
        );
        Ok(())
    }

    /// Seed the enabled-token registry from the zone's current L1 snapshot.
    async fn resolve_and_seed_tokens(
        &mut self,
        l1_provider: &alloy_provider::DynProvider<TempoNetwork>,
        block_number: u64,
    ) -> eyre::Result<()> {
        let portal = self.portal_address;
        let block_id = alloy_rpc_types_eth::BlockId::number(block_number);
        let portal_code = l1_provider
            .get_code_at(portal)
            .block_id(block_id)
            .await
            .map_err(|err| {
                eyre::eyre!(
                    "failed to check portal {portal} deployment at L1 block {block_number}: {err}"
                )
            })?;
        let enabled_tokens = if portal_code.is_empty() {
            info!(
                target: "reth::cli",
                %portal,
                block_number,
                "Portal is not deployed at the L1 anchor, starting with an empty enabled-token registry"
            );
            Vec::new()
        } else {
            ZonePortal::new(portal, l1_provider)
                .enabled_tokens_at(block_id)
                .await
                .map_err(|err| {
                    eyre::eyre!(
                        "failed to discover enabled tokens from portal {portal} at L1 block \
                         {block_number}: {err}"
                    )
                })?
        };
        info!(
            target: "reth::cli",
            count = enabled_tokens.len(),
            ?enabled_tokens,
            block_number,
            "Discovered enabled tokens from L1"
        );

        let mut registry = self.l1_config.enabled_tokens.write();
        registry.clear();
        registry.extend(enabled_tokens);
        Ok(())
    }

    /// Spawn shared L1 observation.
    ///
    /// Sequencers, P2P replicas, and externally driven queue consumers retain finalized blocks
    /// in the deposit queue. A node with none of those only maintains its L1-derived caches and
    /// must not accumulate an unconsumed queue.
    fn spawn_l1_subscriber(&mut self, ctx: &AddOnsContext<'_, N>) {
        if self.sequencer_config.is_some()
            || self.p2p_config.is_some()
            || self.external_deposit_consumer
        {
            L1Subscriber::spawn(
                self.l1_config.clone(),
                ctx.node.provider().clone(),
                self.deposit_queue.clone(),
                ctx.node.task_executor().clone(),
            );
            info!(target: "reth::cli", "L1 subscriber started with deposit enqueueing");
        } else {
            L1Subscriber::spawn_observer(
                self.l1_config.clone(),
                ctx.node.provider().clone(),
                ctx.node.task_executor().clone(),
            );
            info!(target: "reth::cli", "L1 observer started without a deposit sink");
        }
    }

    /// Spawn the [`ZoneEngine`] for L1-event-driven block production.
    fn spawn_zone_engine(
        &self,
        ctx: &AddOnsContext<'_, N>,
        fee_recipient: Address,
        sequencer_key: SecretKey,
    ) -> eyre::Result<()> {
        let provider = ctx.node.provider();
        let last_header = provider
            .sealed_header(provider.best_block_number()?)?
            .ok_or_else(|| eyre::eyre!("no latest block header"))?;
        let engine = ZoneEngine::new(
            provider.chain_spec(),
            ctx.beacon_engine_handle.clone(),
            ctx.node.payload_builder_handle().clone(),
            self.deposit_queue.clone(),
            self.l1_config.block_tracker.clone(),
            last_header,
            fee_recipient,
            sequencer_key,
            self.portal_address,
        );
        ctx.node
            .task_executor()
            .spawn_critical_task("zone-engine", engine.run());
        info!(target: "reth::cli", "ZoneEngine spawned");
        Ok(())
    }

    /// Launch the private RPC server.
    async fn launch_private_rpc(
        config: ZonePrivateRpcConfig,
        handle: &<Self as NodeAddOns<N>>::Handle,
        l1_rpc_url: String,
        retry_connection_interval: Duration,
        portal_address: Address,
        chain_id: u64,
    ) -> eyre::Result<()> {
        if config.zone_id != 0 {
            let expected = zone_primitives::constants::zone_chain_id(config.zone_id);
            if chain_id != expected {
                eyre::bail!(
                    "chain ID mismatch: zone.id={} requires chain_id={}, but genesis has {}",
                    config.zone_id,
                    expected,
                    chain_id,
                );
            }
        }

        let eth_handlers = handle.eth_handlers().clone();
        let zone_rpc_url = handle
            .rpc_server_handles
            .rpc
            .http_url()
            .expect("HTTP RPC server must be enabled for private RPC");
        let private_rpc_config = zone_rpc::PrivateRpcConfig {
            listen_addr: ([0, 0, 0, 0], config.private_rpc_port).into(),
            l1_rpc_url,
            zone_rpc_url,
            retry_connection_interval,
            zone_id: config.zone_id,
            chain_id,
            max_auth_token_validity: config.max_auth_token_validity,
            zone_portal: portal_address,
        };
        let api: Arc<dyn ZoneRpcApi> =
            Arc::new(ZoneRpc::new(eth_handlers, private_rpc_config.clone()).await?);
        let local_addr = start_private_rpc(private_rpc_config, api).await?;
        info!(target: "reth::cli", %local_addr, "Private zone RPC server started");

        Ok(())
    }

    /// Launch sequencer background tasks: batch submission, withdrawal processing,
    /// and engine shutdown hook.
    async fn launch_sequencer_tasks(
        config: ZoneSequencerAddOnsConfig,
        handle: &<Self as NodeAddOns<N>>::Handle,
        task_executor: &reth_tasks::TaskExecutor,
        l1_rpc_url: String,
        portal_address: Address,
        retry_connection_interval: Duration,
        sequencer_addr: Address,
        chain_id: u64,
        attestation_store: Option<AttestationStore>,
    ) -> eyre::Result<()> {
        if config.zone_id != 0 {
            let expected = zone_primitives::constants::zone_chain_id(config.zone_id);
            if chain_id != expected {
                eyre::bail!(
                    "chain ID mismatch: zone.id={} requires chain_id={}, but genesis has {}",
                    config.zone_id,
                    expected,
                    chain_id,
                );
            }
        }

        let zone_rpc_url = handle
            .rpc_server_handles
            .rpc
            .http_url()
            .expect("HTTP RPC server must be enabled for sequencer mode");

        info!(target: "reth::cli", %sequencer_addr, "Starting sequencer background tasks");
        let sequencer_config = ZoneSequencerConfig {
            portal_address,
            l1_rpc_url,
            retry_connection_interval,
            withdrawal_poll_interval: config.withdrawal_poll_interval,
            withdrawal_batch_limits: config.withdrawal_batch_limits,
            outbox_address: ZONE_OUTBOX_ADDRESS,
            inbox_address: ZONE_INBOX_ADDRESS,
            tempo_state_address: TEMPO_STATE_ADDRESS,
            zone_rpc_url,
            zone_poll_interval: config.zone_poll_interval,
            batch_interval_blocks: config.batch_interval_blocks,
            batch_anchor_config: config.batch_anchor_config,
            attestation_store,
        };
        let l1_transaction_signer = config
            .l1_transaction_signer
            .unwrap_or(config.sequencer_signer);
        let seq_handle = spawn_zone_sequencer(sequencer_config, l1_transaction_signer).await;
        info!(target: "reth::cli", "Sequencer tasks spawned");

        // Critical task — node shuts down if either exits.
        task_executor.spawn_critical_task("zone-monitor", async move {
            tokio::select! {
                res = seq_handle.withdrawal_handle => {
                    tracing::error!(target: "reth::cli", ?res, "Withdrawal processor task exited");
                }
                res = seq_handle.monitor_handle => {
                    tracing::error!(target: "reth::cli", ?res, "Zone monitor task exited");
                }
            }
        });

        // Flush unpersisted blocks on shutdown.
        let engine_shutdown = handle.engine_shutdown.clone();
        task_executor.spawn_critical_with_graceful_shutdown_signal(
            "zone-engine-shutdown",
            |shutdown| async move {
                let _guard = shutdown.await;
                info!(target: "reth::cli", "Shutdown signal received — flushing engine state");
                if let Some(done) = engine_shutdown.shutdown() {
                    let _ = done.await;
                }
            },
        );

        Ok(())
    }
}

impl<N> RethRpcAddOns<N> for ZoneAddOns<N>
where
    N: FullNodeComponents<Types = ZoneNode, Evm = ZoneEvmConfig>,
    N::Pool: reth_transaction_pool::TransactionPool<
            Transaction = tempo_transaction_pool::transaction::TempoPooledTransaction,
        >,
    TempoEthApiBuilder<N>:
        EthApiBuilder<N, EthApi: reth_rpc_eth_api::EthApiTypes<NetworkTypes = TempoNetwork>>,
{
    type EthApi = <TempoEthApiBuilder<N> as EthApiBuilder<N>>::EthApi;

    fn hooks_mut(&mut self) -> &mut reth_node_builder::rpc::RpcHooks<N, Self::EthApi> {
        self.inner.hooks_mut()
    }
}

impl<N> EngineValidatorAddOn<N> for ZoneAddOns<N>
where
    N: FullNodeComponents<Types = ZoneNode, Evm = ZoneEvmConfig>,
    N::Pool: reth_transaction_pool::TransactionPool<
            Transaction = tempo_transaction_pool::transaction::TempoPooledTransaction,
        >,
    TempoEthApiBuilder<N>: EthApiBuilder<N, EthApi: EthApiTypes<NetworkTypes = TempoNetwork>>,
{
    type ValidatorBuilder = BasicEngineValidatorBuilder<ZoneEngineValidatorBuilder>;

    fn engine_validator_builder(&self) -> Self::ValidatorBuilder {
        self.inner.engine_validator_builder()
    }
}

impl<N> Node<N> for ZoneNode
where
    N: FullNodeTypes<Types = Self>,
{
    type ComponentsBuilder = ComponentsBuilder<
        N,
        ZonePoolBuilder,
        BasicPayloadServiceBuilder<ZonePayloadFactory>,
        NoopNetworkBuilder<ZoneNetworkPrimitives>,
        ZoneExecutorBuilder,
        NoopConsensusBuilder,
    >;
    type AddOns = ZoneAddOns<NodeAdapter<N>>;

    fn components_builder(&self) -> Self::ComponentsBuilder {
        let executor_builder = ZoneExecutorBuilder::new(
            self.l1_state_provider_config.clone(),
            self.l1_state_cache.clone(),
            self.enabled_tokens.clone(),
            self.l1_block_tracker.clone(),
        );
        let mut payload_factory = ZonePayloadFactory::new(self.withdrawal_batch_interval_blocks);
        if let Some(encryptor) = self.withdrawal_reveal_encryptor.clone() {
            payload_factory = payload_factory.with_withdrawal_reveal_encryptor(encryptor);
        }
        Self::components_with_payload_factory(executor_builder, payload_factory)
    }

    fn add_ons(&self) -> Self::AddOns {
        ZoneAddOns::new(
            self.deposit_queue.clone(),
            self.l1_config.clone(),
            self.portal_address,
            self.private_rpc_config.clone(),
            self.sequencer_config.clone(),
            self.p2p_config.clone(),
            self.external_deposit_consumer,
        )
    }
}

impl<N: FullNodeComponents<Types = Self>> DebugNode<N> for ZoneNode {
    type RpcBlock =
        alloy_rpc_types_eth::Block<alloy_rpc_types_eth::Transaction<TempoTxEnvelope>, TempoHeader>;

    fn rpc_to_primitive_block(rpc_block: Self::RpcBlock) -> primitives::Block {
        rpc_block
            .into_consensus_block()
            .map_transactions(|tx| tx.into_inner())
    }

    fn local_payload_attributes_builder(
        chain_spec: &Self::ChainSpec,
    ) -> impl PayloadAttributesBuilder<<Self::Payload as PayloadTypes>::PayloadAttributes, TempoHeader>
    {
        ZonePayloadAttributesBuilder::new(Arc::new(chain_spec.clone()))
    }
}

/// Builds [`ZonePayloadAttributes`] with `l1_block: None` — suitable for
/// debug/test scenarios where no L1 data is available.
#[derive(Debug)]
pub(crate) struct ZonePayloadAttributesBuilder;

impl ZonePayloadAttributesBuilder {
    pub(crate) fn new(_chain_spec: Arc<ZoneChainSpec>) -> Self {
        Self
    }
}

impl PayloadAttributesBuilder<ZonePayloadAttributes, TempoHeader> for ZonePayloadAttributesBuilder {
    fn build(&self, _parent: &SealedHeader<TempoHeader>) -> ZonePayloadAttributes {
        unimplemented!("zone blocks require L1 data — use ZoneEngine instead")
    }
}

/// Builder that constructs the [`ZoneEvmConfig`] used during block execution.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ZoneExecutorBuilder {
    l1_state_provider_config: L1StateProviderConfig,
    l1_state_cache: L1StateCache,
    enabled_tokens: EnabledTokenRegistry,
    l1_block_tracker: L1BlockTracker,
}

impl ZoneExecutorBuilder {
    /// Create a zone executor builder with the shared L1 state cache.
    ///
    /// `l1_block_tracker` bounds precompile L1 reads to independently observed blocks.
    pub fn new(
        l1_state_provider_config: L1StateProviderConfig,
        l1_state_cache: L1StateCache,
        enabled_tokens: EnabledTokenRegistry,
        l1_block_tracker: L1BlockTracker,
    ) -> Self {
        Self {
            l1_state_provider_config,
            l1_state_cache,
            enabled_tokens,
            l1_block_tracker,
        }
    }
}

impl<Node> ExecutorBuilder<Node> for ZoneExecutorBuilder
where
    Node: FullNodeTypes<Types = ZoneNode>,
{
    type EVM = ZoneEvmConfig;

    async fn build_evm(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::EVM> {
        let runtime_handle = tokio::runtime::Handle::current();
        let portal_address = self.l1_state_provider_config.portal_address;
        let l1_provider = L1StateProvider::new(
            self.l1_state_provider_config.clone(),
            self.l1_state_cache,
            runtime_handle.clone(),
        )
        .await?
        .with_head_bound(self.l1_block_tracker);

        let l1_chain_id = l1_provider.chain_id().await?;
        let tempo_chain_spec = tempo_chain_spec_for_l1(l1_chain_id)
            .ok_or_else(|| eyre::eyre!("unsupported parent Tempo chain ID {l1_chain_id}"))?;
        // Keep the Zone chain settings and use the parent L1 schedule for Tempo hardforks.
        let evm_config = ZoneEvmConfig::new(
            ctx.chain_spec(),
            tempo_chain_spec,
            l1_provider,
            portal_address,
        );
        info!(target: "reth::cli", "Zone EVM initialized with L1-backed Tempo precompiles");

        Ok(evm_config)
    }
}

/// Engine validator builder for Zone.
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct ZoneEngineValidatorBuilder;

impl<Node> PayloadValidatorBuilder<Node> for ZoneEngineValidatorBuilder
where
    Node: FullNodeComponents<Types = ZoneNode>,
{
    type Validator = TempoEngineValidator;

    async fn build(self, _ctx: &AddOnsContext<'_, Node>) -> eyre::Result<Self::Validator> {
        Ok(TempoEngineValidator::new())
    }
}

/// Transaction pool builder for Zone - uses Tempo pool with defaults.
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct ZonePoolBuilder {
    enabled_tokens: EnabledTokenRegistry,
}

impl ZonePoolBuilder {
    /// Create a pool builder using the shared enabled-token registry.
    pub fn new(enabled_tokens: EnabledTokenRegistry) -> Self {
        Self { enabled_tokens }
    }
}

fn validate_has_enabled_token_balance(
    provider: &impl StateProviderFactory,
    enabled_tokens: &EnabledTokenRegistry,
    sender: Address,
) -> Result<(), InvalidPoolTransactionError> {
    let state = provider.latest().map_err(|err| {
        warn!(%err, "Failed to read latest state for zone token-balance admission check");
        InvalidPoolTransactionError::other(TempoPoolTransactionError::Evm(
            TempoInvalidTransaction::EthInvalidTransaction(
                "could not verify balance of an enabled zone token".into(),
            ),
        ))
    })?;

    for token in enabled_tokens.read().iter().copied() {
        let slot = TIP20Token::from_address_unchecked(token).balances[sender].slot();
        let balance = state.storage(token, slot.into()).map_err(|err| {
            warn!(%err, %sender, "Failed to read zone token balance during pool admission");
            InvalidPoolTransactionError::other(TempoPoolTransactionError::Evm(
                TempoInvalidTransaction::EthInvalidTransaction(
                    "could not verify balance of an enabled zone token".into(),
                ),
            ))
        })?;
        if balance.is_some_and(|balance| !balance.is_zero()) {
            return Ok(());
        }
    }

    Err(InvalidPoolTransactionError::other(
        TempoPoolTransactionError::Evm(TempoInvalidTransaction::EthInvalidTransaction(
            "sender must hold a nonzero balance of an enabled zone token".into(),
        )),
    ))
}

impl<Node> PoolBuilder<Node, ZoneEvmConfig> for ZonePoolBuilder
where
    Node: FullNodeTypes<Types = ZoneNode>,
{
    type Pool = TempoTransactionPool<Node::Provider, ZoneEvmConfig>;

    async fn build_pool(
        self,
        ctx: &BuilderContext<Node>,
        evm_config: ZoneEvmConfig,
    ) -> eyre::Result<Self::Pool> {
        // Zone blocks have no protocol base fee, so allow zero-fee transactions into the pool.
        let mut pool_config = ctx.pool_config().with_disabled_protocol_base_fee();
        pool_config.max_inflight_delegated_slot_limit = pool_config.max_account_slots;

        // this store is effectively a noop
        let blob_store = InMemoryBlobStore::default();
        let additional_tasks = ctx.config().txpool.additional_validation_tasks;
        let task_executor = ctx.task_executor().clone();
        let mut validator =
            TransactionValidationTaskExecutor::eth_builder(ctx.provider().clone(), evm_config)
                .with_max_tx_input_bytes(ctx.config().txpool.max_tx_input_bytes)
                .with_local_transactions_config(pool_config.local_transactions_config.clone())
                .set_tx_fee_cap(ctx.config().rpc.rpc_tx_fee_cap)
                .with_max_tx_gas_limit(ctx.config().txpool.max_tx_gas_limit)
                .set_block_gas_limit(ctx.chain_spec().genesis().gas_limit)
                .disable_balance_check()
                .with_minimum_priority_fee(ctx.config().txpool.minimum_priority_fee)
                .with_custom_tx_type(TempoTxType::AA as u8)
                .no_eip7702()
                .no_eip4844()
                .build::<TempoPooledTransaction, _>(blob_store.clone());

        validator.set_additional_stateless_validation(|_origin, tx| {
            zone_evm::validate_transaction(
                tx.tx_env(),
                zone_primitives::constants::CONTRACT_DEPLOYER_ALLOWLIST,
            )
            .map_err(|err| InvalidPoolTransactionError::other(TempoPoolTransactionError::Evm(err)))
        });

        let provider = ctx.provider().clone();
        let enabled_tokens = self.enabled_tokens;
        validator.set_additional_stateful_validation(move |_origin, tx, _account_state| {
            validate_has_enabled_token_balance(&provider, &enabled_tokens, *tx.sender_ref())
        });

        let validator =
            TransactionValidationTaskExecutor::spawn(validator, &task_executor, additional_tasks);

        let aa_2d_config = AA2dPoolConfig {
            price_bump_config: pool_config.price_bumps,
            pending_limit: pool_config.pending_limit,
            queued_limit: pool_config.queued_limit,
            max_txs_per_sender: pool_config.max_account_slots,
        };
        let aa_2d_pool = AA2dPool::new(aa_2d_config);
        let amm_liquidity_cache = AmmLiquidityCache::new(ctx.provider())?;

        let validator = validator.map(|v| {
            TempoTransactionValidator::new(
                v,
                DEFAULT_AA_VALID_AFTER_MAX_SECS,
                DEFAULT_MAX_TEMPO_AUTHORIZATIONS,
                amm_liquidity_cache.clone(),
            )
            // Zones collect the selected fee token directly and never route through FeeAMM.
            .with_disable_fee_amm_check(true)
        });
        let protocol_pool = Pool::new(
            validator,
            TempoTipOrdering::default(),
            blob_store,
            pool_config.clone(),
        );

        let transaction_pool = TempoTransactionPool::new(protocol_pool, aa_2d_pool);

        spawn_maintenance_tasks(ctx, transaction_pool.clone(), &pool_config)?;

        // Spawn unified Tempo pool maintenance task
        // This consolidates: expired AA txs, 2D nonce updates, AMM cache, and keychain revocations
        ctx.task_executor().spawn_critical_task(
            "txpool maintenance - tempo pool",
            tempo_transaction_pool::maintain::maintain_tempo_pool(transaction_pool.clone()),
        );

        info!(target: "reth::cli", "Transaction pool initialized");
        debug!(target: "reth::cli", "Spawned txpool maintenance task");

        Ok(transaction_pool)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{Signed, TxEip1559};
    use alloy_primitives::{Bytes, Signature, TxKind, U256};
    use reth_chainspec::EthChainSpec;
    use reth_primitives_traits::Recovered;
    use tempo_primitives::transaction::{
        AASigned, Call, PrimitiveSignature, TempoSignature, TempoTransaction,
    };

    fn pooled_transaction(envelope: TempoTxEnvelope, sender: Address) -> TempoPooledTransaction {
        TempoPooledTransaction::new(Recovered::new_unchecked(envelope, sender))
    }

    fn aa_transaction(sender: Address, calls: Vec<Call>) -> TempoPooledTransaction {
        let transaction = TempoTransaction {
            calls,
            ..Default::default()
        };
        let signature =
            TempoSignature::Primitive(PrimitiveSignature::Secp256k1(Signature::test_signature()));
        pooled_transaction(
            AASigned::new_unhashed(transaction, signature).into(),
            sender,
        )
    }

    #[test]
    fn resolves_public_and_local_tempo_l1_specs() {
        assert_eq!(tempo_chain_spec_for_l1(4217).unwrap().chain().id(), 4217);
        assert_eq!(tempo_chain_spec_for_l1(42431).unwrap().chain().id(), 42431);
        assert_eq!(tempo_chain_spec_for_l1(1337).unwrap().chain().id(), 1337);
        assert_eq!(tempo_chain_spec_for_l1(31337).unwrap().chain().id(), 1337);
        assert!(tempo_chain_spec_for_l1(999_999).is_none());

        // SAFETY: test-only env mutation; no other test reads this variable.
        unsafe { std::env::set_var("ZONE_L1_DEV_CHAIN_IDS", "31318, 31319") };
        assert_eq!(tempo_chain_spec_for_l1(31318).unwrap().chain().id(), 1337);
        assert_eq!(tempo_chain_spec_for_l1(31319).unwrap().chain().id(), 1337);
        assert!(tempo_chain_spec_for_l1(999_999).is_none());
        unsafe { std::env::remove_var("ZONE_L1_DEV_CHAIN_IDS") };
    }

    #[test]
    fn pool_policy_allows_allowlisted_plain_create() {
        let sender = Address::repeat_byte(0x11);
        let envelope = TempoTxEnvelope::Eip1559(Signed::new_unhashed(
            TxEip1559 {
                to: TxKind::Create,
                ..Default::default()
            },
            Signature::test_signature(),
        ));
        let transaction = pooled_transaction(envelope, sender);

        let err = zone_evm::validate_transaction(transaction.tx_env(), &[]).unwrap_err();
        assert!(matches!(
            err,
            tempo_revm::TempoInvalidTransaction::CallsValidation(_)
        ));
        assert!(zone_evm::validate_transaction(transaction.tx_env(), &[sender]).is_ok());
    }

    #[test]
    fn pool_policy_rejects_create_in_non_first_aa_call() {
        let transaction = aa_transaction(
            Address::repeat_byte(0x11),
            vec![
                Call {
                    to: TxKind::Call(Address::repeat_byte(0x22)),
                    value: U256::ZERO,
                    input: Bytes::new(),
                },
                Call {
                    to: TxKind::Create,
                    value: U256::ZERO,
                    input: Bytes::new(),
                },
            ],
        );

        assert!(zone_evm::validate_transaction(transaction.tx_env(), &[]).is_err());
    }
}
