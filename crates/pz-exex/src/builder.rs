//! Builder for PzNode.

use crate::{PzNode, PzNodeTypes, types::PzNodeTypesDb};
use reth_db_common::init;
use reth_exex::ExExContext;
use reth_node_api::FullNodeComponents;
use reth_provider::{BlockHashReader, ProviderFactory};
use std::path::PathBuf;
use std::sync::Arc;

/// Marker for no database set.
#[derive(Debug, Clone, Copy)]
pub struct NoDb;

/// Builder for [`PzNode`].
#[allow(private_interfaces)]
pub struct PzNodeBuilder<Host = (), Db = NoDb> {
    ctx: Option<Host>,
    factory: Option<Db>,
    chain_spec: Option<Arc<reth_chainspec::ChainSpec>>,
    data_dir: Option<PathBuf>,
}

impl<Host, Db> std::fmt::Debug for PzNodeBuilder<Host, Db> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PzNodeBuilder").finish_non_exhaustive()
    }
}

impl Default for PzNodeBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl PzNodeBuilder {
    /// Create a new builder.
    pub fn new() -> Self {
        Self {
            ctx: None,
            factory: None,
            chain_spec: None,
            data_dir: None,
        }
    }
}

impl<Host, Db> PzNodeBuilder<Host, Db> {
    /// Set the ExEx context.
    pub fn with_ctx<NewHost>(
        self,
        ctx: ExExContext<NewHost>,
    ) -> PzNodeBuilder<ExExContext<NewHost>, Db>
    where
        NewHost: FullNodeComponents,
    {
        PzNodeBuilder {
            ctx: Some(ctx),
            factory: self.factory,
            chain_spec: self.chain_spec,
            data_dir: self.data_dir,
        }
    }

    /// Set the chain spec.
    pub fn with_chain_spec(mut self, chain_spec: Arc<reth_chainspec::ChainSpec>) -> Self {
        self.chain_spec = Some(chain_spec);
        self
    }

    /// Set the data directory.
    pub fn with_data_dir(mut self, path: PathBuf) -> Self {
        self.data_dir = Some(path);
        self
    }
}

impl<Host> PzNodeBuilder<Host, NoDb> {
    /// Set the provider factory directly.
    pub fn with_factory<NewDb: PzNodeTypesDb>(
        self,
        factory: ProviderFactory<PzNodeTypes<NewDb>>,
    ) -> PzNodeBuilder<Host, ProviderFactory<PzNodeTypes<NewDb>>> {
        PzNodeBuilder {
            ctx: self.ctx,
            factory: Some(factory),
            chain_spec: self.chain_spec,
            data_dir: self.data_dir,
        }
    }
}

impl<Host> PzNodeBuilder<ExExContext<Host>, NoDb>
where
    Host: FullNodeComponents,
{
    /// Build with a new MDBX database at the configured data directory.
    pub fn build(self) -> eyre::Result<PzNode<Host, Arc<reth_db::DatabaseEnv>>> {
        let data_dir = self
            .data_dir
            .ok_or_else(|| eyre::eyre!("data_dir must be set"))?;
        let chain_spec = self
            .chain_spec
            .ok_or_else(|| eyre::eyre!("chain_spec must be set"))?;

        // Create database
        let db_path = data_dir.join("db");
        std::fs::create_dir_all(&db_path)?;
        let db = reth_db::init_db(db_path, reth_db::mdbx::DatabaseArguments::default())?;
        
        // Create static file provider
        let static_files_path = data_dir.join("static_files");
        std::fs::create_dir_all(&static_files_path)?;
        let static_file_provider =
            reth_provider::providers::StaticFileProvider::read_write(static_files_path)?;

        // Create rocksdb provider
        let rocksdb_path = data_dir.join("rocksdb");
        std::fs::create_dir_all(&rocksdb_path)?;
        let rocksdb = reth_provider::providers::RocksDBProvider::builder(rocksdb_path).build()?;

        // Create provider factory
        let factory = ProviderFactory::<PzNodeTypes<Arc<reth_db::DatabaseEnv>>>::new(
            Arc::new(db),
            chain_spec.clone(),
            static_file_provider,
            rocksdb,
        )?;

        // Initialize genesis if needed
        Self::init_genesis(&factory)?;

        let ctx = self.ctx.ok_or_else(|| eyre::eyre!("ctx must be set"))?;
        PzNode::new_unsafe(ctx, factory, chain_spec)
    }

    /// Initialize genesis state if the database is empty.
    fn init_genesis(
        factory: &ProviderFactory<PzNodeTypes<Arc<reth_db::DatabaseEnv>>>,
    ) -> eyre::Result<()> {
        // Check if genesis already exists
        if factory.block_hash(0).is_ok_and(|h| h.is_some()) {
            return Ok(());
        }

        // Initialize genesis
        init::init_genesis(factory)?;

        // TODO: Clear trie tables we don't need (like Signet does)
        // For now, just init genesis without clearing

        Ok(())
    }
}

impl<Host, Db> PzNodeBuilder<ExExContext<Host>, ProviderFactory<PzNodeTypes<Db>>>
where
    Host: FullNodeComponents,
    Db: PzNodeTypesDb,
{
    /// Build with a pre-configured provider factory.
    pub fn build_with_factory(self) -> eyre::Result<PzNode<Host, Db>> {
        let ctx = self.ctx.ok_or_else(|| eyre::eyre!("ctx must be set"))?;
        let factory = self
            .factory
            .ok_or_else(|| eyre::eyre!("factory must be set"))?;
        let chain_spec = self
            .chain_spec
            .ok_or_else(|| eyre::eyre!("chain_spec must be set"))?;

        PzNode::new_unsafe(ctx, factory, chain_spec)
    }
}
