use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    future::Future,
    io,
    pin::Pin,
    sync::{Arc, Mutex, RwLock, Weak},
    time::Duration,
};

use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_signer::SignerSync as _;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::SolValue;
use openraft::{
    BasicNode, Config,
    network::RPCOption,
    raft::{
        AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest,
        InstallSnapshotResponse, VoteRequest, VoteResponse,
    },
};
use serde::{Deserialize, Serialize};
use tempfile::TempDir;
use tokio::time::{sleep, timeout};
use zone_fast_transfer::{CertificateError, DurableJournal, EpochRoster, QuorumVerifier};
use zone_node::{
    fast_quorum::{
        AuthenticatedRaftPeer, FAST_PROTOCOL_VERSION, FastActivation, FastRaftConfig,
        FastRaftRuntime, FinalizedFastEpoch, FinalizedT14Capability, RaftCommit, RaftTransport,
        SigningError, TransportError, assemble_fast_raft, sign_committed_outcome,
        t14_fast_protocol_native_pin, verify_committed_certificate,
    },
    fast_raft_state_machine::{
        AppliedBlock, CommittedTransferRecord, DurableStateMachineExecution,
    },
};
use zone_primitives::fast_transfer::{
    AssetId, CertificateBody, OutcomeCertificate, SignatureBytes, TransferIntent, TransferOutcome,
    ZoneDomain,
};

const WAIT: Duration = Duration::from_secs(8);
const INITIAL_POOL: u64 = 100;

type Node = FastRaftRuntime<NativeExecutor>;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct NativePayment {
    business_nonce: u64,
    recipient: Address,
    amount: u64,
    body: B256,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
struct NativeState {
    pool: u64,
    balances: BTreeMap<Address, u64>,
    nonces: BTreeMap<u64, B256>,
    receipts: Vec<bool>,
}

impl NativeState {
    fn genesis() -> Self {
        Self {
            pool: INITIAL_POOL,
            ..Self::default()
        }
    }

    fn execute(&mut self, input: &zone_node::fast_quorum::ReplicatedBlockInput) -> io::Result<()> {
        for bytes in &input.transactions {
            let payment: NativePayment = bincode::deserialize(bytes)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            match self.nonces.get(&payment.business_nonce) {
                Some(body) if *body == payment.body => self.receipts.push(true),
                Some(_) => self.receipts.push(false),
                None if self.pool >= payment.amount => {
                    self.pool -= payment.amount;
                    *self.balances.entry(payment.recipient).or_default() += payment.amount;
                    self.nonces.insert(payment.business_nonce, payment.body);
                    self.receipts.push(true);
                }
                None => self.receipts.push(false),
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
struct NativeExecutor {
    journal: Arc<DurableJournal>,
    state: Mutex<NativeState>,
    applied: Mutex<Vec<AppliedBlock>>,
}

impl NativeExecutor {
    fn open(path: &std::path::Path) -> io::Result<Self> {
        let journal = DurableJournal::open(path.join("replay"))
            .map_err(|error| io::Error::other(error.to_string()))?;
        Ok(Self {
            journal: Arc::new(journal),
            state: Mutex::new(NativeState::genesis()),
            applied: Mutex::new(Vec::new()),
        })
    }

    fn state(&self) -> NativeState {
        self.state.lock().unwrap().clone()
    }

    fn applied(&self) -> Vec<AppliedBlock> {
        self.applied.lock().unwrap().clone()
    }

    fn output(
        log_id: openraft::LogId<u64>,
        input: &zone_node::fast_quorum::ReplicatedBlockInput,
        state: &NativeState,
    ) -> zone_node::fast_quorum::CommittedBlock {
        let state_bytes = bincode::serialize(state).unwrap();
        let coordinate = bincode::serialize(&(log_id.leader_id.term, log_id.index)).unwrap();
        zone_node::fast_quorum::CommittedBlock {
            input_digest: input.digest(),
            block_height: log_id.index,
            block_hash: keccak256([coordinate.as_slice(), input.digest().as_slice()].concat()),
            state_root: keccak256(&state_bytes),
            receipts_root: keccak256(bincode::serialize(&state.receipts).unwrap()),
        }
    }

    fn replay(blocks: &[AppliedBlock]) -> io::Result<NativeState> {
        let mut state = NativeState::genesis();
        for block in blocks {
            state.execute(&block.input)?;
            if Self::output(block.log_id, &block.input, &state) != block.output {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "committed replay output differs from durable state-machine image",
                ));
            }
        }
        Ok(state)
    }
}

impl DurableStateMachineExecution for NativeExecutor {
    type Error = io::Error;

    fn apply_committed<'a>(
        &'a self,
        log_id: openraft::LogId<u64>,
        input: &'a zone_node::fast_quorum::ReplicatedBlockInput,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<zone_node::fast_quorum::CommittedBlock, Self::Error>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let mut next = self.state();
            next.execute(input)?;
            let output = Self::output(log_id, input, &next);
            self.journal
                .persist_replicated_block(zone_fast_transfer::ReplicatedBlockInput {
                    log_term: log_id.leader_id.term,
                    log_index: log_id.index,
                    block_height: output.block_height,
                    block_hash: output.block_hash,
                    state_root: output.state_root,
                    block_input: input.block_input.to_vec(),
                    transactions: input.transactions.iter().map(|tx| tx.to_vec()).collect(),
                    l1_execution_input: input.l1_inputs.to_vec(),
                    witness: input.replay_witness.to_vec(),
                })
                .map_err(|error| io::Error::other(error.to_string()))?;
            *self.state.lock().unwrap() = next;
            self.applied.lock().unwrap().push(AppliedBlock {
                log_id,
                input: input.clone(),
                output: output.clone(),
            });
            Ok(output)
        })
    }

    fn restore_committed<'a>(
        &'a self,
        blocks: &'a [AppliedBlock],
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        Box::pin(async move {
            let replay = self
                .journal
                .replicated_blocks_from(0)
                .map_err(|error| io::Error::other(error.to_string()))?;
            for block in blocks {
                let retained = replay
                    .iter()
                    .find(|record| record.log_index == block.log_id.index)
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "committed state-machine entry is missing replay history",
                        )
                    })?;
                if retained.log_term != block.log_id.leader_id.term
                    || retained.block_hash != block.output.block_hash
                    || retained.state_root != block.output.state_root
                    || retained.transactions
                        != block
                            .input
                            .transactions
                            .iter()
                            .map(|tx| tx.to_vec())
                            .collect::<Vec<_>>()
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "committed replay history conflicts with state-machine image",
                    ));
                }
            }
            *self.state.lock().unwrap() = Self::replay(blocks)?;
            *self.applied.lock().unwrap() = blocks.to_vec();
            Ok(())
        })
    }

    fn committed_transfer(
        &self,
        _transfer_id: B256,
    ) -> Result<Option<CommittedTransferRecord>, Self::Error> {
        Ok(None)
    }
}

#[derive(Default)]
struct NetworkState {
    nodes: RwLock<BTreeMap<u64, Weak<Node>>>,
    blocked: Mutex<HashSet<(u64, u64)>>,
    drop_append_response: Mutex<HashSet<(u64, u64)>>,
}

impl NetworkState {
    fn register(&self, id: u64, node: &Arc<Node>) {
        self.nodes.write().unwrap().insert(id, Arc::downgrade(node));
    }

    fn unregister(&self, id: u64) {
        self.nodes.write().unwrap().remove(&id);
    }

    fn node(&self, source: u64, target: u64) -> Result<Arc<Node>, TransportError> {
        if self.blocked.lock().unwrap().contains(&(source, target)) {
            return Err(TransportError {
                message: format!("partitioned {source}->{target}"),
            });
        }
        self.nodes
            .read()
            .unwrap()
            .get(&target)
            .and_then(Weak::upgrade)
            .ok_or_else(|| TransportError {
                message: format!("replica {target} unavailable"),
            })
    }

    fn isolate(&self, id: u64) {
        let mut blocked = self.blocked.lock().unwrap();
        for peer in 1..=3 {
            if peer != id {
                blocked.insert((id, peer));
                blocked.insert((peer, id));
            }
        }
    }

    fn heal(&self) {
        self.blocked.lock().unwrap().clear();
        self.drop_append_response.lock().unwrap().clear();
    }
}

#[derive(Clone)]
struct TestTransport {
    source: u64,
    epoch: u64,
    members: [Address; 3],
    network: Arc<NetworkState>,
}

impl TestTransport {
    fn peer(&self) -> AuthenticatedRaftPeer {
        AuthenticatedRaftPeer {
            epoch: self.epoch,
            node_id: self.source,
            member: self.members[(self.source - 1) as usize],
        }
    }
}

impl RaftTransport for TestTransport {
    fn append_entries(
        &self,
        target: u64,
        _node: &BasicNode,
        request: AppendEntriesRequest<FastRaftConfig>,
        _deadline: RPCOption,
    ) -> Pin<Box<dyn Future<Output = Result<AppendEntriesResponse<u64>, TransportError>> + Send + '_>>
    {
        Box::pin(async move {
            let node = self.network.node(self.source, target)?;
            let response = node
                .handle_append_entries(self.peer(), request)
                .await
                .map_err(|error| TransportError {
                    message: error.to_string(),
                })?;
            if self
                .network
                .drop_append_response
                .lock()
                .unwrap()
                .remove(&(self.source, target))
            {
                return Err(TransportError {
                    message: "append was fsynced but its response was lost".to_owned(),
                });
            }
            Ok(response)
        })
    }

    fn vote(
        &self,
        target: u64,
        _node: &BasicNode,
        request: VoteRequest<u64>,
        _deadline: RPCOption,
    ) -> Pin<Box<dyn Future<Output = Result<VoteResponse<u64>, TransportError>> + Send + '_>> {
        Box::pin(async move {
            self.network
                .node(self.source, target)?
                .handle_vote(self.peer(), request)
                .await
                .map_err(|error| TransportError {
                    message: error.to_string(),
                })
        })
    }

    fn install_snapshot(
        &self,
        target: u64,
        _node: &BasicNode,
        request: InstallSnapshotRequest<FastRaftConfig>,
        _deadline: RPCOption,
    ) -> Pin<
        Box<dyn Future<Output = Result<InstallSnapshotResponse<u64>, TransportError>> + Send + '_>,
    > {
        Box::pin(async move {
            self.network
                .node(self.source, target)?
                .handle_install_snapshot(self.peer(), request)
                .await
                .map_err(|error| TransportError {
                    message: error.to_string(),
                })
        })
    }
}

struct Cluster {
    activation: FastActivation,
    signers: [PrivateKeySigner; 3],
    network: Arc<NetworkState>,
    directories: [TempDir; 3],
    nodes: Vec<Option<Arc<Node>>>,
    executors: Vec<Option<Arc<NativeExecutor>>>,
}

impl Cluster {
    async fn new() -> Self {
        let signers = [0x31, 0x32, 0x33]
            .map(|byte| PrivateKeySigner::from_bytes(&B256::with_last_byte(byte)).unwrap());
        let activation = activation(signers.each_ref().map(|signer| signer.address()));
        let mut cluster = Self {
            activation,
            signers,
            network: Arc::new(NetworkState::default()),
            directories: std::array::from_fn(|_| tempfile::tempdir().unwrap()),
            nodes: vec![None, None, None],
            executors: vec![None, None, None],
        };
        for id in 1..=3 {
            cluster.start(id).await.unwrap();
        }
        let membership = (1_u64..=3)
            .map(|id| (id, BasicNode::new(format!("memory://replica-{id}"))))
            .collect::<BTreeMap<_, _>>();
        cluster
            .node(1)
            .raft
            .inner()
            .initialize(membership)
            .await
            .unwrap();
        cluster.wait_for_leader_excluding(&BTreeSet::new()).await;
        cluster
    }

    fn config(&self) -> Arc<Config> {
        Arc::new(
            Config {
                cluster_name: "t14-consensus-acceptance".to_owned(),
                election_timeout_min: 250,
                election_timeout_max: 500,
                heartbeat_interval: 50,
                install_snapshot_timeout: 1_000,
                ..Config::default()
            }
            .validate()
            .unwrap(),
        )
    }

    async fn start(&mut self, id: u64) -> io::Result<()> {
        let directory = self.directories[(id - 1) as usize].path();
        let executor = Arc::new(NativeExecutor::open(directory)?);
        let transport = Arc::new(TestTransport {
            source: id,
            epoch: self.activation.epoch().epoch,
            members: self.activation.epoch().members,
            network: self.network.clone(),
        });
        let runtime = Arc::new(
            assemble_fast_raft(
                &self.activation,
                self.activation.epoch().members[(id - 1) as usize],
                self.config(),
                transport,
                directory,
                executor.clone(),
            )
            .await
            .map_err(|error| io::Error::other(error.to_string()))?,
        );
        self.network.register(id, &runtime);
        self.nodes[(id - 1) as usize] = Some(runtime);
        self.executors[(id - 1) as usize] = Some(executor);
        Ok(())
    }

    async fn stop(&mut self, id: u64) {
        self.network.unregister(id);
        if let Some(node) = self.nodes[(id - 1) as usize].take() {
            timeout(WAIT, node.raft.inner().shutdown())
                .await
                .expect("bounded Raft shutdown")
                .unwrap();
        }
        self.executors[(id - 1) as usize] = None;
    }

    fn node(&self, id: u64) -> &Arc<Node> {
        self.nodes[(id - 1) as usize].as_ref().unwrap()
    }

    fn executor(&self, id: u64) -> &Arc<NativeExecutor> {
        self.executors[(id - 1) as usize].as_ref().unwrap()
    }

    async fn wait_for_leader_excluding(&self, excluded: &BTreeSet<u64>) -> u64 {
        timeout(WAIT, async {
            loop {
                for id in 1..=3 {
                    if excluded.contains(&id) || self.nodes[(id - 1) as usize].is_none() {
                        continue;
                    }
                    let node = self.node(id);
                    if node.raft.inner().current_leader().await == Some(id)
                        && node.raft.inner().ensure_linearizable().await.is_ok()
                    {
                        return id;
                    }
                }
                sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("cluster elects a quorum leader within bound")
    }

    async fn wait_head(&self, id: u64, index: u64) {
        timeout(WAIT, async {
            loop {
                if self
                    .node(id)
                    .committed_head()
                    .unwrap()
                    .is_some_and(|head| head.log_id.index >= index)
                {
                    return;
                }
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("replica applies committed head within bound");
    }

    async fn commit(&self, input: zone_node::fast_quorum::ReplicatedBlockInput) -> RaftCommit {
        let leader = self.wait_for_leader_excluding(&BTreeSet::new()).await;
        timeout(WAIT, self.node(leader).raft.commit(input))
            .await
            .expect("client write completes within bound")
            .unwrap()
    }

    async fn shutdown_all(&mut self) {
        for id in 1..=3 {
            if self.nodes[(id - 1) as usize].is_some() {
                self.stop(id).await;
            }
        }
    }
}

fn activation(members: [Address; 3]) -> FastActivation {
    let portal = Address::repeat_byte(0x20);
    let peer_portals = std::array::from_fn(|index| Address::with_last_byte(0x40 + index as u8));
    let roster_hash = keccak256(
        (
            keccak256("TEMPO_ZONE_FAST_ROSTER_T14_V1"),
            portal,
            7_u64,
            FAST_PROTOCOL_VERSION,
            U256::from(2),
            U256::from(1),
            B256::repeat_byte(21),
            B256::repeat_byte(22),
            members.to_vec(),
            peer_portals.to_vec(),
        )
            .abi_encode(),
    );
    FastActivation::from_finalized_epoch(FinalizedT14Capability {
        epoch: FinalizedFastEpoch {
            l1_chain_id: 42,
            portal,
            zone_id: 2,
            zone_chain_id: 4_002,
            epoch: 7,
            protocol_version: FAST_PROTOCOL_VERSION,
            threshold: 2,
            proof_mode: 1,
            expected_verifier_code_hash: B256::repeat_byte(21),
            expected_verifier_config_hash: B256::repeat_byte(22),
            members,
            peer_portals,
            roster_hash,
            finalized_l1_block: 100,
        },
        native_pin: t14_fast_protocol_native_pin(),
        t14_active_at_anchor: true,
        current_epoch: 7,
        activated_at_l1_block: 99,
        closed: false,
        retired: false,
    })
    .unwrap()
}

fn payment(nonce: u64, recipient: Address, amount: u64, body_byte: u8) -> NativePayment {
    NativePayment {
        business_nonce: nonce,
        recipient,
        amount,
        body: B256::repeat_byte(body_byte),
    }
}

fn block(
    seed: u8,
    payments: impl IntoIterator<Item = NativePayment>,
) -> zone_node::fast_quorum::ReplicatedBlockInput {
    zone_node::fast_quorum::ReplicatedBlockInput {
        epoch: 7,
        parent_hash: B256::repeat_byte(seed.wrapping_sub(1)),
        block_input: Bytes::from(vec![seed, 0xa1]),
        transactions: payments
            .into_iter()
            .map(|payment| Bytes::from(bincode::serialize(&payment).unwrap()))
            .collect(),
        l1_inputs: Bytes::from(vec![seed, 0xb2]),
        replay_witness: Bytes::from(vec![seed, 0xc3, 0xd4]),
    }
}

fn destination_domain(cluster: &Cluster) -> ZoneDomain {
    let epoch = cluster.activation.epoch();
    ZoneDomain {
        l1_chain_id: epoch.l1_chain_id,
        zone_id: epoch.zone_id,
        chain_id: epoch.zone_chain_id,
        portal: epoch.portal,
        authority_epoch: epoch.epoch,
        roster_hash: epoch.roster_hash,
        protocol_version: epoch.protocol_version as u16,
    }
}

fn intent(cluster: &Cluster) -> TransferIntent {
    let destination = destination_domain(cluster);
    TransferIntent {
        source: ZoneDomain {
            zone_id: 1,
            chain_id: 4_001,
            portal: Address::repeat_byte(0x10),
            roster_hash: B256::repeat_byte(0x10),
            ..destination
        },
        destination,
        asset: AssetId {
            l1_token: Address::repeat_byte(0x44),
            source_token: Address::repeat_byte(0x45),
            destination_token: Address::repeat_byte(0x46),
            decimals: 6,
        },
        sender: Address::repeat_byte(0x51),
        recipient: Address::repeat_byte(0x52),
        refund_account: Address::repeat_byte(0x51),
        destination_pool: Address::repeat_byte(0x53),
        reimbursement_account: Address::repeat_byte(0x54),
        principal: U256::from(10),
        fee: U256::ZERO,
        quote_id: B256::repeat_byte(0x55),
        destination_expiry_height: 900,
        transfer_nonce: 19,
    }
}

fn certificate_body(intent: &TransferIntent, commit: &RaftCommit) -> CertificateBody {
    CertificateBody {
        transfer_id: intent.transfer_id(),
        intent_hash: intent.intent_hash(),
        zone: intent.destination,
        log_term: commit.term,
        log_index: commit.index,
        block_height: commit.block.block_height,
        block_hash: commit.block.block_hash,
        state_root: commit.block.state_root,
        transaction_hash: B256::repeat_byte(0x63),
        outcome: TransferOutcome::Paid {
            pool: intent.destination_pool,
            recipient: intent.recipient,
            principal: intent.principal,
        },
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_replica_commit_partition_failover_and_retry_are_canonical() {
    let mut cluster = Cluster::new().await;
    let recipient = Address::repeat_byte(0x70);
    let leader = cluster.wait_for_leader_excluding(&BTreeSet::new()).await;
    let isolated_follower = (1..=3).find(|id| *id != leader).unwrap();
    cluster.network.isolate(isolated_follower);

    let retry = payment(41, recipient, 10, 0x41);
    let commit = timeout(
        WAIT,
        cluster.node(leader).raft.commit(block(1, [retry.clone()])),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(cluster.executor(leader).state().pool, 90);
    assert_eq!(cluster.executor(leader).state().balances[&recipient], 10);
    assert_eq!(
        cluster.executor(isolated_follower).state(),
        NativeState::genesis()
    );
    assert!(
        cluster
            .node(isolated_follower)
            .committed_head()
            .unwrap()
            .is_none()
    );

    cluster.network.heal();
    for id in 1..=3 {
        cluster.wait_head(id, commit.index).await;
    }
    let heads = (1..=3)
        .map(|id| cluster.node(id).committed_head().unwrap().unwrap())
        .collect::<Vec<_>>();
    assert!(heads.windows(2).all(|pair| pair[0] == pair[1]));
    assert!((1..=3).all(|id| cluster.executor(id).state() == cluster.executor(1).state()));

    let retry_leader = cluster.wait_for_leader_excluding(&BTreeSet::new()).await;
    let first_client = cluster.node(retry_leader).raft.clone();
    let second_client = first_client.clone();
    let (first_retry, second_retry) = tokio::join!(
        first_client.commit(block(2, [retry.clone()])),
        second_client.commit(block(3, [retry]))
    );
    let first_retry = first_retry.unwrap();
    let second_retry = second_retry.unwrap();
    let retry_index = first_retry.index.max(second_retry.index);
    for id in 1..=3 {
        cluster.wait_head(id, retry_index).await;
        assert_eq!(cluster.executor(id).state().pool, 90);
        assert_eq!(cluster.executor(id).state().balances[&recipient], 10);
    }

    let old_head = cluster
        .node(retry_leader)
        .committed_head()
        .unwrap()
        .unwrap();
    cluster.network.isolate(retry_leader);
    let false_commit = timeout(
        Duration::from_millis(700),
        cluster
            .node(retry_leader)
            .raft
            .commit(block(4, [payment(42, recipient, 7, 0x42)])),
    )
    .await;
    assert!(false_commit.is_err() || false_commit.unwrap().is_err());
    assert_eq!(
        cluster
            .node(retry_leader)
            .committed_head()
            .unwrap()
            .unwrap(),
        old_head
    );
    assert_eq!(
        cluster.executor(retry_leader).state().balances[&recipient],
        10
    );

    let majority = (1..=3)
        .filter(|id| *id != retry_leader)
        .collect::<BTreeSet<_>>();
    let new_leader = cluster
        .wait_for_leader_excluding(&BTreeSet::from([retry_leader]))
        .await;
    assert!(majority.contains(&new_leader));
    let failover = cluster
        .node(new_leader)
        .raft
        .commit(block(5, [payment(43, recipient, 7, 0x43)]))
        .await
        .unwrap();
    assert_eq!(
        cluster.executor(new_leader).state().balances[&recipient],
        17
    );
    cluster.network.heal();
    for id in 1..=3 {
        cluster.wait_head(id, failover.index).await;
        assert_eq!(cluster.executor(id).state().balances[&recipient], 17);
    }
    cluster.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_boundaries_recover_the_original_fsynced_entry_coordinates() {
    let mut cluster = Cluster::new().await;
    let recipient = Address::repeat_byte(0x71);

    let leader = cluster.wait_for_leader_excluding(&BTreeSet::new()).await;
    cluster.stop(leader).await;
    let elected = cluster
        .wait_for_leader_excluding(&BTreeSet::from([leader]))
        .await;
    let before_append = cluster
        .node(elected)
        .raft
        .commit(block(10, [payment(50, recipient, 5, 0x50)]))
        .await
        .unwrap();
    assert_eq!(cluster.executor(elected).state().balances[&recipient], 5);
    cluster.start(leader).await.unwrap();
    cluster.wait_head(leader, before_append.index).await;

    let leader = cluster.wait_for_leader_excluding(&BTreeSet::new()).await;
    let mut followers = (1..=3).filter(|id| *id != leader);
    let retained = followers.next().unwrap();
    let unavailable = followers.next().unwrap();
    cluster.network.isolate(unavailable);
    cluster
        .network
        .drop_append_response
        .lock()
        .unwrap()
        .insert((leader, retained));
    let pending_input = block(11, [payment(51, recipient, 6, 0x51)]);
    let pending_digest = pending_input.digest();
    let leader_handle = cluster.node(leader).raft.clone();
    let pending = tokio::spawn(async move { leader_handle.commit(pending_input).await });

    let previous = before_append.index;
    timeout(WAIT, async {
        loop {
            if cluster
                .node(retained)
                .raft
                .inner()
                .metrics()
                .borrow()
                .last_log_index
                .is_some_and(|index| index > previous)
            {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("follower fsyncs pending append within bound");
    assert_eq!(cluster.executor(retained).state().balances[&recipient], 5);
    cluster.stop(leader).await;
    pending.abort();
    cluster.network.heal();
    cluster
        .node(retained)
        .raft
        .inner()
        .trigger()
        .elect()
        .await
        .unwrap();
    let recovered_leader = cluster
        .wait_for_leader_excluding(&BTreeSet::from([leader]))
        .await;
    assert_eq!(recovered_leader, retained);
    timeout(WAIT, async {
        loop {
            if cluster.executor(retained).state().balances.get(&recipient) == Some(&11) {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("new leader commits retained old-term entry");
    let recovered = cluster.node(retained).committed_head().unwrap().unwrap();
    let applied = cluster
        .executor(retained)
        .applied()
        .into_iter()
        .find(|entry| entry.input.digest() == pending_digest)
        .expect("retained entry is replayed from its original bytes");
    assert_eq!(applied.log_id, recovered.log_id);
    assert_eq!(applied.output.input_digest, pending_digest);
    cluster.start(leader).await.unwrap();
    cluster.wait_head(leader, recovered.log_id.index).await;
    cluster.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pending_entry_cannot_create_a_signing_record() {
    let mut cluster = Cluster::new().await;
    let transfer = intent(&cluster);
    let leader = cluster.wait_for_leader_excluding(&BTreeSet::new()).await;
    let fabricated = RaftCommit {
        term: 99,
        index: 99,
        block: NativeExecutor::output(
            openraft::LogId::new(openraft::CommittedLeaderId::new(99, leader), 99),
            &block(20, [payment(60, transfer.recipient, 10, 0x60)]),
            &NativeState::genesis(),
        ),
    };
    let pending_journal = DurableJournal::open(
        cluster.directories[(leader - 1) as usize]
            .path()
            .join("pending-signing"),
    )
    .unwrap();
    let pending = sign_committed_outcome(
        &pending_journal,
        &cluster.activation,
        &fabricated,
        cluster.signers[(leader - 1) as usize].address(),
        |_| certificate_body(&transfer, &fabricated),
        |_| SignatureBytes([7; 65]),
    );
    assert!(
        pending.is_err(),
        "an outcome absent from the durable committed replay prefix must not be signed"
    );
    let digest = {
        let verifier = QuorumVerifier::new(
            EpochRoster::from_finalized_registry(
                transfer.destination,
                cluster.signers.each_ref().map(|signer| signer.address()),
            )
            .unwrap(),
        );
        verifier.outcome_digest(&OutcomeCertificate {
            body: certificate_body(&transfer, &fabricated),
            signatures: [SignatureBytes([0; 65]); 2],
        })
    };
    assert!(
        pending_journal
            .signing_record(digest, cluster.signers[(leader - 1) as usize].address())
            .unwrap()
            .is_none()
    );
    assert!(cluster.node(leader).committed_head().unwrap().is_none());
    cluster.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn certification_requires_committed_replay_and_two_matching_durable_signers() {
    let mut cluster = Cluster::new().await;
    let transfer = intent(&cluster);
    let leader = cluster.wait_for_leader_excluding(&BTreeSet::new()).await;
    let commit = cluster
        .node(leader)
        .raft
        .commit(block(21, [payment(61, transfer.recipient, 10, 0x61)]))
        .await
        .unwrap();
    let original = commit.clone();
    cluster.stop(leader).await;
    let replacement = cluster
        .wait_for_leader_excluding(&BTreeSet::from([leader]))
        .await;
    cluster.wait_head(replacement, commit.index).await;
    let recovered_head = cluster.node(replacement).committed_head().unwrap().unwrap();
    assert_eq!(recovered_head.log_id.leader_id.term, original.term);
    assert_eq!(recovered_head.log_id.index, original.index);
    assert_eq!(recovered_head.block, original.block);

    let body = certificate_body(&transfer, &commit);
    let roster = EpochRoster::from_finalized_registry(
        transfer.destination,
        cluster.signers.each_ref().map(|signer| signer.address()),
    )
    .unwrap();
    let verifier = QuorumVerifier::new(roster);
    let unsigned = OutcomeCertificate {
        body: body.clone(),
        signatures: [SignatureBytes([0; 65]); 2],
    };
    let digest = verifier.outcome_digest(&unsigned);
    let signer_ids = (1..=3).filter(|id| *id != leader).collect::<Vec<_>>();
    let mut signatures = Vec::new();
    for id in &signer_ids {
        let journal = &cluster.executor(*id).journal;
        let signer = &cluster.signers[(*id - 1) as usize];
        let signature = sign_committed_outcome(
            journal.as_ref(),
            &cluster.activation,
            &commit,
            signer.address(),
            |_| body.clone(),
            |signed_digest| {
                assert_eq!(signed_digest, digest);
                SignatureBytes(signer.sign_hash_sync(&signed_digest).unwrap().as_bytes())
            },
        )
        .unwrap();
        assert!(
            journal
                .signing_record(digest, signer.address())
                .unwrap()
                .is_some()
        );
        signatures.push(signature);
    }
    let certificate = OutcomeCertificate {
        body: body.clone(),
        signatures: [signatures[0], signatures[1]],
    };
    assert_eq!(
        verify_committed_certificate(&verifier, &certificate, &transfer, &commit).unwrap(),
        [
            cluster.signers[(signer_ids[0] - 1) as usize].address(),
            cluster.signers[(signer_ids[1] - 1) as usize].address(),
        ]
    );

    let duplicate = OutcomeCertificate {
        body: body.clone(),
        signatures: [signatures[0], signatures[0]],
    };
    assert_eq!(
        verifier.verify_outcome(&duplicate, &transfer),
        Err(CertificateError::DuplicateSigner)
    );
    let mut mixed = certificate;
    mixed.body.transaction_hash = B256::repeat_byte(0xee);
    assert!(verifier.verify_outcome(&mixed, &transfer).is_err());

    let mismatch = sign_committed_outcome(
        cluster.executor(replacement).journal.as_ref(),
        &cluster.activation,
        &commit,
        cluster.signers[(replacement - 1) as usize].address(),
        |_| {
            let mut changed = body;
            changed.log_index += 1;
            changed
        },
        |_| SignatureBytes([0; 65]),
    );
    assert!(matches!(
        mismatch,
        Err(SigningError::OutcomeDoesNotMatchCommit)
    ));
    cluster.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn snapshot_restart_restores_exact_prefix_and_bad_history_fails_closed() {
    let mut cluster = Cluster::new().await;
    let recipient = Address::repeat_byte(0x72);
    let mut last = None;
    for seed in 30..34 {
        last = Some(
            cluster
                .commit(block(seed, [payment(u64::from(seed), recipient, 3, seed)]))
                .await,
        );
    }
    let last = last.unwrap();
    for id in 1..=3 {
        cluster.wait_head(id, last.index).await;
    }
    let leader = cluster.wait_for_leader_excluding(&BTreeSet::new()).await;
    cluster
        .node(leader)
        .raft
        .inner()
        .trigger()
        .snapshot()
        .await
        .unwrap();
    timeout(WAIT, async {
        loop {
            if cluster
                .node(leader)
                .raft
                .inner()
                .metrics()
                .borrow()
                .snapshot
                .is_some_and(|id| id.index >= last.index)
            {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("snapshot is durably built within bound");

    let restart = leader;
    let expected_state = cluster.executor(restart).state();
    let expected_prefix = cluster.executor(restart).applied();
    cluster.stop(restart).await;
    cluster.start(restart).await.unwrap();
    assert_eq!(cluster.executor(restart).state(), expected_state);
    assert_eq!(cluster.executor(restart).applied(), expected_prefix);
    assert_eq!(
        cluster
            .node(restart)
            .committed_head()
            .unwrap()
            .unwrap()
            .block,
        last.block
    );

    let corrupt = (1..=3).find(|id| *id != restart).unwrap();
    cluster.stop(corrupt).await;
    let state_path = cluster.directories[(corrupt - 1) as usize]
        .path()
        .join("state-machine/raft-state-machine.bin");
    let mut bytes = std::fs::read(&state_path).unwrap();
    *bytes.last_mut().unwrap() ^= 1;
    std::fs::write(&state_path, bytes).unwrap();
    let error = cluster.start(corrupt).await.unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Other);
    assert!(error.to_string().contains("state-machine") || error.to_string().contains("checksum"));

    cluster.stop(restart).await;
    let replay_path = cluster.directories[(restart - 1) as usize]
        .path()
        .join("replay/journal.bin");
    std::fs::write(replay_path, []).unwrap();
    let missing = cluster.start(restart).await.unwrap_err();
    assert!(missing.to_string().contains("missing replay history"));

    cluster.shutdown_all().await;
}
