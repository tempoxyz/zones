//! Privacy Zone node types for reth integration.

use reth_chainspec::ChainSpec;
use reth_db::{Database, database_metrics::DatabaseMetrics};
use reth_node_api::{NodePrimitives, NodeTypes, NodeTypesWithDB};
use reth_node_ethereum::EthEngineTypes;
use reth_primitives::EthPrimitives;
use reth_provider::EthStorage;
use std::fmt::Debug;
use std::marker::PhantomData;

/// Trait alias for database types that work with PzNodeTypes.
pub trait PzNodeTypesDb: Database + DatabaseMetrics + Clone + Unpin + 'static {}
impl<T: Database + DatabaseMetrics + Clone + Unpin + 'static> PzNodeTypesDb for T {}

/// Privacy Zone node types for [`NodeTypes`] and [`NodeTypesWithDB`].
///
/// Uses Ethereum primitives since the L2 is EVM-compatible.
#[derive(Debug)]
pub struct PzNodeTypes<Db> {
    _db: PhantomData<fn() -> Db>,
}

impl<Db> Clone for PzNodeTypes<Db> {
    fn clone(&self) -> Self {
        Self { _db: PhantomData }
    }
}

impl<Db> Copy for PzNodeTypes<Db> {}

impl<Db> Default for PzNodeTypes<Db> {
    fn default() -> Self {
        Self { _db: PhantomData }
    }
}

impl<Db> PartialEq for PzNodeTypes<Db> {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl<Db> Eq for PzNodeTypes<Db> {}

impl<Db: PzNodeTypesDb> NodePrimitives for PzNodeTypes<Db> {
    type Block = <EthPrimitives as NodePrimitives>::Block;
    type BlockHeader = <EthPrimitives as NodePrimitives>::BlockHeader;
    type BlockBody = <EthPrimitives as NodePrimitives>::BlockBody;
    type SignedTx = <EthPrimitives as NodePrimitives>::SignedTx;
    type Receipt = <EthPrimitives as NodePrimitives>::Receipt;
}

impl<Db: PzNodeTypesDb> NodeTypes for PzNodeTypes<Db> {
    type Primitives = EthPrimitives;
    type ChainSpec = ChainSpec;
    type Storage = EthStorage;
    type Payload = EthEngineTypes;
}

impl<Db: PzNodeTypesDb> NodeTypesWithDB for PzNodeTypes<Db> {
    type DB = Db;
}
