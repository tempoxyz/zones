//! Privacy Zone L2 Node.
//!
//! Listens to L1 ExEx notifications, extracts deposits, and processes L2 blocks.

use crate::{PzNodeTypes, processor::PzBlockProcessor, types::PzNodeTypesDb};
use alloy_consensus::BlockHeader;
use alloy_primitives::{B256, b256};
use eyre::Context;
use futures_util::StreamExt;
use reth_exex::{ExExContext, ExExEvent, ExExHead};
use reth_node_api::{FullNodeComponents, NodeTypes};
use reth_primitives::EthPrimitives;
use reth_provider::{
    BlockNumReader, BlockReader, CanonStateNotifications, CanonStateSubscriptions, Chain,
    HeaderProvider, NodePrimitivesProvider, ProviderFactory, providers::BlockchainProvider,
};
use std::fmt;
use std::sync::Arc;
use tracing::{debug, info, instrument};

/// Genesis journal hash for the L2 chain.
pub(crate) const GENESIS_JOURNAL_HASH: B256 =
    b256!("0x0000000000000000000000000000000000000000000000000000000000000000");

/// Make it easier to write some args.
type PrimitivesOf<Host> = <<Host as reth_node_api::FullNodeTypes>::Types as NodeTypes>::Primitives;
type HostChain<Host> = Chain<PrimitivesOf<Host>>;

/// Privacy Zone L2 Node.
///
/// Listens to L1 (host) chain notifications via ExEx and processes L2 blocks.
pub struct PzNode<Host, Db>
where
    Host: FullNodeComponents,
    Host::Types: NodeTypes<Primitives = EthPrimitives>,
    Db: PzNodeTypesDb,
{
    /// The host ExEx context for receiving L1 notifications.
    host: ExExContext<Host>,

    /// L2 provider factory for database access.
    l2_provider: ProviderFactory<PzNodeTypes<Db>>,

    /// L2 blockchain provider for RPC and state queries.
    l2_bp: BlockchainProvider<PzNodeTypes<Db>>,

    /// Block processor for executing L2 blocks.
    processor: PzBlockProcessor<Db>,

    /// L2 chain spec.
    #[allow(dead_code)] // Will be used for RPC methods
    chain_spec: Arc<reth_chainspec::ChainSpec>,
}

impl<Host, Db> fmt::Debug for PzNode<Host, Db>
where
    Host: FullNodeComponents,
    Host::Types: NodeTypes<Primitives = EthPrimitives>,
    Db: PzNodeTypesDb,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PzNode").finish_non_exhaustive()
    }
}

impl<Host, Db> NodePrimitivesProvider for PzNode<Host, Db>
where
    Host: FullNodeComponents,
    Host::Types: NodeTypes<Primitives = EthPrimitives>,
    Db: PzNodeTypesDb,
{
    type Primitives = EthPrimitives;
}

impl<Host, Db> CanonStateSubscriptions for PzNode<Host, Db>
where
    Host: FullNodeComponents,
    Host::Types: NodeTypes<Primitives = EthPrimitives>,
    Db: PzNodeTypesDb,
{
    fn subscribe_to_canonical_state(&self) -> CanonStateNotifications<Self::Primitives> {
        self.l2_bp.subscribe_to_canonical_state()
    }
}

impl<Host, Db> PzNode<Host, Db>
where
    Host: FullNodeComponents,
    Host::Types: NodeTypes<Primitives = EthPrimitives>,
    Db: PzNodeTypesDb,
{
    /// Create a new PzNode.
    ///
    /// # Safety
    /// Genesis must be initialized before calling this.
    /// Use [`PzNodeBuilder`] instead for safe construction.
    #[doc(hidden)]
    // TODO: this is not unsafe, update
    pub fn new_unsafe(
        ctx: ExExContext<Host>,
        factory: ProviderFactory<PzNodeTypes<Db>>,
        chain_spec: Arc<reth_chainspec::ChainSpec>,
    ) -> eyre::Result<Self> {
        let l2_bp = BlockchainProvider::new(factory.clone())?;
        let processor = PzBlockProcessor::new(chain_spec.clone(), factory.clone());

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
    pub async fn start(mut self) -> eyre::Result<()> {
        info!("Starting Privacy Zone L2 node");

        // Get last L2 block for resumption
        let last_l2_block = self.l2_provider.last_block_number()?;
        info!(last_l2_block, "Resuming from last L2 block");

        // Set ExEx head
        let exex_head = self.set_exex_head(last_l2_block)?;
        info!(
            host_head = exex_head.block.number,
            host_hash = %exex_head.block.hash,
            l2_head = last_l2_block,
            "PZ L2 listening for L1 notifications"
        );

        // Main event loop
        while let Some(notification) = self.host.notifications.next().await {
            let notification = notification.wrap_err("error in L1 notifications stream")?;
            self.on_notification(notification)
                .await
                .wrap_err("error processing notification")?;
        }

        info!("Privacy Zone L2 shutting down");
        Ok(())
    }

    /// Set the ExEx head based on last L2 block.
    fn set_exex_head(&mut self, last_l2_block: u64) -> eyre::Result<ExExHead> {
        // For now, just use genesis block as our starting point
        // TODO: Implement proper L1<->L2 block mapping
        if last_l2_block == 0 {
            if let Some(genesis_block) = self.host.provider().block_by_number(0)? {
                let exex_head = ExExHead {
                    block: genesis_block.num_hash_slow(),
                };
                self.host.set_notifications_without_head();
                return Ok(exex_head);
            }
        }

        // Get current L1 head
        let l1_head = self.host.provider().last_block_number()?;
        let l1_header = self
            .host
            .provider()
            .sealed_header(l1_head)?
            .ok_or_else(|| eyre::eyre!("missing L1 header"))?;

        let exex_head = ExExHead {
            block: l1_header.num_hash(),
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
        self.processor.on_l1_commit::<Host>(chain).await?;

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
