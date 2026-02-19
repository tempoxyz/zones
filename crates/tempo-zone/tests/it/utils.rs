use alloy_primitives::{Address, B256, keccak256};
use alloy_sol_types::SolValue;
use reth_ethereum::tasks::TaskManager;
use reth_node_api::FullNodeComponents;
use reth_node_builder::{NodeBuilder, NodeConfig, NodeHandle, rpc::RethRpcAddOns};
use reth_node_core::args::RpcServerArgs;
use reth_rpc_builder::RpcModuleSelection;
use std::{sync::Arc, time::Duration};
use tempo_chainspec::spec::TempoChainSpec;
use zone::{DepositQueue, L1SubscriberConfig, ZoneNode, witness::SharedWitnessStore};

pub(crate) const TEST_MNEMONIC: &str =
    "test test test test test test test test test test test junk";

/// Deterministic salt for the zone test token.
pub(crate) const ZONE_TEST_TOKEN_SALT: B256 = B256::new([
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
]);

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

pub(crate) struct ZoneTestNode {
    pub deposit_queue: DepositQueue,
    pub http_url: url::Url,
    _node_handle: Box<dyn TestNodeHandle>,
    _tasks: TaskManager,
}

impl ZoneTestNode {
    pub(crate) async fn start(
        l1_ws_url: String,
        portal_address: Address,
        token_address: Address,
    ) -> eyre::Result<Self> {
        let tasks = TaskManager::current();

        let genesis: serde_json::Value =
            serde_json::from_str(include_str!("../assets/test-genesis.json"))?;
        let chain_spec = TempoChainSpec::from_genesis(serde_json::from_value(genesis)?);

        let deposit_queue = DepositQueue::default();

        let l1_config = L1SubscriberConfig {
            l1_rpc_url: l1_ws_url,
            portal_address,
            genesis_tempo_block_number: None,
        };
        // TODO: provide real L1 state config when integration tests use proofs
        let l1_state_provider_config = Default::default();
        let l1_state_listener_config = Default::default();
        let l1_state_cache = Default::default();
        let witness_store: SharedWitnessStore = Default::default();
        let _ = token_address; // TODO: wire token address into genesis
        let zone_node = ZoneNode::new(
            deposit_queue.clone(),
            l1_config,
            l1_state_provider_config,
            l1_state_listener_config,
            l1_state_cache,
            None,
            witness_store,
        );

        let mut node_config = NodeConfig::new(Arc::new(chain_spec))
            .with_unused_ports()
            .dev()
            .with_rpc(
                RpcServerArgs::default()
                    .with_unused_ports()
                    .with_http()
                    .with_http_api(RpcModuleSelection::All),
            );
        node_config.dev.block_time = Some(Duration::from_millis(250));

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

        Ok(Self {
            deposit_queue,
            http_url,
            _node_handle: Box::new(node_handle),
            _tasks: tasks,
        })
    }
}
