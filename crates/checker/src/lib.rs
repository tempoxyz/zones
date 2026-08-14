//! Durable observe-only checker for one Tempo Zone.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod adapter;
mod bootstrap;
mod exex;
mod failure;
mod kernel;
mod metrics;
mod observe;
pub(crate) mod persistence;
mod runtime;

pub mod inspection;

use std::{fmt, future::Future, path::PathBuf, str::FromStr, time::Duration};

use alloy_primitives::Address;
use reth_exex::ExExContext;
use reth_node_api::{FullNodeComponents, NodeTypes};
use reth_storage_api::{BlockNumReader, BlockReader, StateProviderFactory};
use tempo_primitives::{Block, TempoPrimitives};

pub use bootstrap::build_checkpoint;

/// Why the checker stopped verifying canonical Zone history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckerBlockedReason {
    /// The node delivered a malformed or discontinuous notification sequence.
    InvalidNotificationSequence,
    /// The configured Tempo provider belongs to another chain.
    TempoChainMismatch,
    /// Authenticated work violated an internal checker assumption.
    InvalidAuthenticatedData,
    /// A Zone reorg precedes the locally retained checker history.
    DeepReorgBeyondRetention,
    /// Acquisition lag exceeded the bounded ExEx journal retention policy.
    AcquisitionLagExceeded,
}

impl fmt::Display for CheckerBlockedReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reason = match self {
            Self::InvalidNotificationSequence => "invalid notification sequence",
            Self::TempoChainMismatch => "Tempo chain mismatch",
            Self::InvalidAuthenticatedData => "invalid authenticated data",
            Self::DeepReorgBeyondRetention => "reorg exceeds retained checker history",
            Self::AcquisitionLagExceeded => "acquisition lag exceeds retention bound",
        };
        f.write_str(reason)
    }
}

/// Runtime mode for the checker ExEx.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CheckerMode {
    /// Checker is not installed.
    #[default]
    Off,
    /// Checker authenticates observations and persists its semantic state and
    /// findings without affecting block execution.
    Observe,
}

impl fmt::Display for CheckerMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Off => "off",
            Self::Observe => "observe",
        })
    }
}

impl FromStr for CheckerMode {
    type Err = eyre::Report;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "off" => Ok(Self::Off),
            "observe" => Ok(Self::Observe),
            other => Err(eyre::eyre!(
                "invalid checker mode `{other}`, expected `off` or `observe`"
            )),
        }
    }
}

/// Configuration for one checker database and Portal.
#[derive(Debug, Clone)]
pub struct CheckerConfig {
    /// Archive-capable Tempo RPC used for bootstrap and live checks.
    pub l1_rpc_url: String,
    /// ZonePortal checked by this instance.
    pub portal_address: Address,
    /// ZoneFactory Zone ID bound to the local Zone chain ID.
    pub zone_id: u32,
    /// Checker database path.
    pub database_path: PathBuf,
    /// Maximum time for one block acquisition attempt.
    pub acquisition_timeout: Duration,
}

/// Checker ExEx configuration.
pub struct CheckerExEx {
    config: CheckerConfig,
}

impl CheckerExEx {
    /// Create a checker ExEx from node configuration.
    pub const fn new(config: CheckerConfig) -> Self {
        Self { config }
    }

    /// Run preflight and return the checker worker.
    pub fn launch<Node>(
        self,
        ctx: ExExContext<Node>,
    ) -> eyre::Result<impl Future<Output = eyre::Result<()>> + Send>
    where
        Node: FullNodeComponents,
        Node::Provider: BlockReader<Block = Block> + BlockNumReader + StateProviderFactory,
        Node::Types: NodeTypes<Primitives = TempoPrimitives>,
    {
        Ok(exex::run(self.config, ctx))
    }
}
