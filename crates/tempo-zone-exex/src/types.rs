//! Tempo Zone node types for reth integration.

use reth_chainspec::ChainSpec;
use reth_db::{Database, database_metrics::DatabaseMetrics};
use reth_node_api::{NodePrimitives, NodeTypes, NodeTypesWithDB};
use reth_node_ethereum::EthEngineTypes;
use reth_primitives::EthPrimitives;
use reth_provider::EthStorage;
use std::fmt::Debug;
use std::marker::PhantomData;

/// Trait alias for database types that work with ZoneNodeTypes.
pub trait ZoneNodeTypesDb: Database + DatabaseMetrics + Clone + Unpin + 'static {}
impl<T: Database + DatabaseMetrics + Clone + Unpin + 'static> ZoneNodeTypesDb for T {}

/// Tempo Zone node types for [`NodeTypes`] and [`NodeTypesWithDB`].
///
/// Uses Ethereum primitives since the L2 is EVM-compatible.
#[derive(Debug)]
pub struct ZoneNodeTypes<Db> {
    _db: PhantomData<fn() -> Db>,
}

impl<Db> Clone for ZoneNodeTypes<Db> {
    fn clone(&self) -> Self {
        Self { _db: PhantomData }
    }
}

impl<Db> Copy for ZoneNodeTypes<Db> {}

impl<Db> Default for ZoneNodeTypes<Db> {
    fn default() -> Self {
        Self { _db: PhantomData }
    }
}

impl<Db> PartialEq for ZoneNodeTypes<Db> {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl<Db> Eq for ZoneNodeTypes<Db> {}

impl<Db: ZoneNodeTypesDb> NodePrimitives for ZoneNodeTypes<Db> {
    type Block = <EthPrimitives as NodePrimitives>::Block;
    type BlockHeader = <EthPrimitives as NodePrimitives>::BlockHeader;
    type BlockBody = <EthPrimitives as NodePrimitives>::BlockBody;
    type SignedTx = <EthPrimitives as NodePrimitives>::SignedTx;
    type Receipt = <EthPrimitives as NodePrimitives>::Receipt;
}

impl<Db: ZoneNodeTypesDb> NodeTypes for ZoneNodeTypes<Db> {
    type Primitives = EthPrimitives;
    type ChainSpec = ChainSpec;
    type Storage = EthStorage;
    type Payload = EthEngineTypes;
}

impl<Db: ZoneNodeTypesDb> NodeTypesWithDB for ZoneNodeTypes<Db> {
    type DB = Db;
}
