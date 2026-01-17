//! Tempo Zone L2 Node.
//!
//! Listens to L1 ExEx notifications, extracts deposits, and processes L2 blocks.

use crate::{
    BlockBuilder, BlockBuilderConfig, BlockBuilderHandle,
    ZoneNodeTypes, processor::ZoneBlockProcessor, types::ZoneNodeTypesDb,
};
use alloy_consensus::BlockHeader;
use alloy_primitives::{B256, b256};
use eyre::Context;
use futures_util::StreamExt;
use reth_exex::{ExExContext, ExExEvent, ExExHead};
use reth_node_api::{FullNodeComponents, NodeTypes};
use reth_primitives::EthPrimitives;
use reth_primitives_traits::NodePrimitives;
use reth_provider::{
    BlockNumReader, CanonStateNotifications, CanonStateSubscriptions, Chain, HeaderProvider,
    NodePrimitivesProvider, ProviderFactory, providers::BlockchainProvider,
};
use std::fmt;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, info, instrument, warn};

/// Genesis journal hash for the L2 chain.
#[allow(dead_code)] // Will be used when journal tracking is implemented
pub(crate) const GENESIS_JOURNAL_HASH: B256 =
    b256!("0x0000000000000000000000000000000000000000000000000000000000000000");

/// Make it easier to write some args.
type PrimitivesOf<Host> = <<Host as reth_node_api::FullNodeTypes>::Types as NodeTypes>::Primitives;
type HostChain<Host> = Chain<PrimitivesOf<Host>>;

/// Tempo Zone L2 Node.
///
/// Listens to L1 (host) chain notifications via ExEx and processes L2 blocks.
pub struct ZoneNode<Host, Db>
where
    Host: FullNodeComponents,
    Db: ZoneNodeTypesDb,
{
    /// The host ExEx context for receiving L1 notifications.
    host: ExExContext<Host>,

    /// L2 provider factory for database access.
    l2_provider: ProviderFactory<ZoneNodeTypes<Db>>,

    /// L2 blockchain provider for RPC and state queries.
    l2_bp: BlockchainProvider<ZoneNodeTypes<Db>>,

    /// Block processor for executing L2 blocks.
    processor: ZoneBlockProcessor<Db>,

    /// L2 chain spec.
    #[allow(dead_code)] // Will be used for RPC methods
    chain_spec: Arc<reth_chainspec::ChainSpec>,
}

impl<Host, Db> fmt::Debug for ZoneNode<Host, Db>
where
    Host: FullNodeComponents,
    Db: ZoneNodeTypesDb,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ZoneNode").finish_non_exhaustive()
    }
}

impl<Host, Db> NodePrimitivesProvider for ZoneNode<Host, Db>
where
    Host: FullNodeComponents,
    Db: ZoneNodeTypesDb,
{
    type Primitives = EthPrimitives;
}

impl<Host, Db> CanonStateSubscriptions for ZoneNode<Host, Db>
where
    Host: FullNodeComponents,
    Db: ZoneNodeTypesDb,
{
    fn subscribe_to_canonical_state(&self) -> CanonStateNotifications<Self::Primitives> {
        self.l2_bp.subscribe_to_canonical_state()
    }
}

impl<Host, Db> ZoneNode<Host, Db>
where
    Host: FullNodeComponents,
    PrimitivesOf<Host>: NodePrimitives,
    Db: ZoneNodeTypesDb,
{
    /// Create a new ZoneNode.
    ///
    /// # Safety
    /// Genesis must be initialized before calling this.
    /// Use [`ZoneNodeBuilder`] instead for safe construction.
    #[doc(hidden)]
    // TODO: this is not unsafe, update
    pub fn new_unsafe(
        ctx: ExExContext<Host>,
        factory: ProviderFactory<ZoneNodeTypes<Db>>,
        chain_spec: Arc<reth_chainspec::ChainSpec>,
    ) -> eyre::Result<Self> {
        let l2_bp = BlockchainProvider::new(factory.clone())?;
        let processor = ZoneBlockProcessor::new(chain_spec.clone(), factory.clone());

        Ok(Self {
            host: ctx,
            l2_provider: factory,
            l2_bp,
            processor,
            chain_spec,
        })
    }

    /// Start the L2 node, listening for L1 notifications.
    #[instrument(skip(self))]
    pub async fn start(self) -> eyre::Result<()> {
        self.start_with_config(BlockBuilderConfig::default()).await
    }

    /// Start the L2 node with custom block builder configuration.
    #[instrument(skip(self, config))]
    pub async fn start_with_config(mut self, config: BlockBuilderConfig) -> eyre::Result<()> {
        info!("Starting Tempo Zone L2 node");

        // Get last L2 block for resumption
        let last_l2_block = self.l2_provider.last_block_number()?;
        info!(last_l2_block, "Resuming from last L2 block");

        // Set ExEx head
        let exex_head = self.set_exex_head(last_l2_block)?;
        info!(
            host_head = exex_head.block.number,
            host_hash = %exex_head.block.hash,
            l2_head = last_l2_block,
            "Tempo Zone L2 listening for L1 notifications"
        );

        // Create channel for receiving produced blocks
        let (block_tx, mut block_rx) = mpsc::channel(16);

        // Create and spawn the block builder
        let (block_builder, tx_handle) = BlockBuilder::new(
            config,
            self.processor.chain_spec(),
            self.processor.l2_db(),
            block_tx,
        );
        let builder_task = block_builder.spawn();
        info!("Block builder spawned");

        // Store the tx handle for submitting transactions (e.g., from RPC)
        let _tx_handle = tx_handle;

        // Main event loop - process L1 notifications and produced blocks
        loop {
            tokio::select! {
                notification = self.host.notifications.next() => {
                    match notification {
                        Some(Ok(notification)) => {
                            self.on_notification(notification)
                                .await
                                .wrap_err("error processing notification")?;
                        }
                        Some(Err(e)) => {
                            return Err(e).wrap_err("error in L1 notifications stream");
                        }
                        None => {
                            info!("L1 notifications stream ended");
                            break;
                        }
                    }
                }
                Some(block_notification) = block_rx.recv() => {
                    debug!(
                        block_number = block_notification.block.header().number,
                        block_hash = %block_notification.block.hash(),
                        "Received produced block from builder"
                    );
                    // Block is already persisted by the builder, just log for now
                    // Future: broadcast to peers, update RPC state, etc.
                }
            }
        }

        // Cleanup
        builder_task.abort();
        info!("Tempo Zone L2 shutting down");
        Ok(())
    }

    /// Get a handle to submit transactions to the block builder.
    ///
    /// Note: This must be called after start() is invoked. For now, use
    /// `start_with_handle()` to get the handle before starting.
    pub fn start_with_handle(
        self,
        config: BlockBuilderConfig,
    ) -> (BlockBuilderHandle, impl std::future::Future<Output = eyre::Result<()>>) {
        let (block_tx, block_rx) = mpsc::channel(16);

        let (block_builder, tx_handle) = BlockBuilder::new(
            config,
            self.processor.chain_spec(),
            self.processor.l2_db(),
            block_tx,
        );

        let future = Self::run_with_builder(self, block_builder, block_rx);

        (tx_handle, future)
    }

    async fn run_with_builder(
        mut self,
        block_builder: BlockBuilder,
        mut block_rx: mpsc::Receiver<crate::NewBlockNotification>,
    ) -> eyre::Result<()> {
        info!("Starting Tempo Zone L2 node");

        let last_l2_block = self.l2_provider.last_block_number()?;
        info!(last_l2_block, "Resuming from last L2 block");

        let exex_head = self.set_exex_head(last_l2_block)?;
        info!(
            host_head = exex_head.block.number,
            host_hash = %exex_head.block.hash,
            l2_head = last_l2_block,
            "Tempo Zone L2 listening for L1 notifications"
        );

        let builder_task = block_builder.spawn();
        info!("Block builder spawned");

        loop {
            tokio::select! {
                notification = self.host.notifications.next() => {
                    match notification {
                        Some(Ok(notification)) => {
                            self.on_notification(notification)
                                .await
                                .wrap_err("error processing notification")?;
                        }
                        Some(Err(e)) => {
                            return Err(e).wrap_err("error in L1 notifications stream");
                        }
                        None => {
                            info!("L1 notifications stream ended");
                            break;
                        }
                    }
                }
                Some(block_notification) = block_rx.recv() => {
                    debug!(
                        block_number = block_notification.block.header().number,
                        block_hash = %block_notification.block.hash(),
                        "Received produced block from builder"
                    );
                }
            }
        }

        builder_task.abort();
        info!("Tempo Zone L2 shutting down");
        Ok(())
    }

    /// Set the ExEx head based on last L2 block.
    fn set_exex_head(&mut self, last_l2_block: u64) -> eyre::Result<ExExHead> {
        // For now, just use genesis block as our starting point
        // TODO: Implement proper L1<->L2 block mapping
        let block_number = if last_l2_block == 0 { 0 } else { last_l2_block };

        // Get the sealed header at the target block number
        let header = self
            .host
            .provider()
            .sealed_header(block_number)?
            .ok_or_else(|| eyre::eyre!("missing L1 header at block {}", block_number))?;

        let exex_head = ExExHead {
            block: header.num_hash(),
        };
        self.host.set_notifications_without_head();
        Ok(exex_head)
    }

    /// Handle an ExEx notification (commit or revert).
    async fn on_notification(
        &mut self,
        notification: reth_exex::ExExNotification<PrimitivesOf<Host>>,
    ) -> eyre::Result<()> {
        // Handle reverts first
        if let Some(reverted) = notification.reverted_chain() {
            self.on_revert(&reverted)?;
        }

        // Then handle commits
        if let Some(committed) = notification.committed_chain() {
            self.on_commit(&committed).await?;

            // Notify ExEx that we've processed up to this height
            self.host
                .events
                .send(ExExEvent::FinishedHeight(committed.tip().num_hash()))?;
        }

        Ok(())
    }

    /// Handle committed L1 blocks - extract deposits and build L2 blocks.
    #[instrument(skip_all, fields(first = chain.first().header().number(), tip = chain.tip().header().number()))]
    async fn on_commit(&mut self, chain: &Arc<HostChain<Host>>) -> eyre::Result<()> {
        debug!(blocks = chain.len(), "Processing L1 commit");

        // Process each L1 block to build corresponding L2 blocks
        self.processor.on_l1_commit(chain).await?;

        Ok(())
    }

    /// Handle reverted L1 blocks - unwind L2 state.
    #[instrument(skip_all, fields(first = chain.first().header().number(), tip = chain.tip().header().number()))]
    fn on_revert(&mut self, chain: &Arc<HostChain<Host>>) -> eyre::Result<()> {
        debug!(blocks = chain.len(), "Processing L1 revert");

        // TODO: Implement L2 revert logic
        // For now just log
        tracing::warn!("L1 revert handling not yet implemented");

        Ok(())
    }
}
