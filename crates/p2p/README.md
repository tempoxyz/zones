# Tempo Zone P2P

This crate provides the static networking and the leadership schedule for a
multi-sequencer Tempo Zone. A manifest defines the static peer topology (names, addresses,
Ed25519 identities, individual secp256k1 addresses). Each node loads its own Commonware
Ed25519 identity and validates that it appears in the manifest.

Which member *leads* (produces blocks) is not decided by the manifest: normally it is derived
from finalized Tempo L1 state — the `ZonePortal`'s `leader`, `leaderEpoch`, and
`leaderActivationTempoBlock` fields and its `LeaderUpdated` event. Every observed transition
is retained in an activation-indexed `LeadershipSchedule`.

For manual crashed-leader recovery, the operator stops the nodes, selects a canonical tip shared by
the survivors, adds the same `[forced_recovery]` directive to every manifest, and restarts them.
Each node verifies that its local canonical head has the configured hash before any role task
starts. The selected replacement governs from the next Tempo anchor until the first subsequent
finalized portal transition reaches its activation anchor. Nodes never submit that transition
automatically: the operator may call the ordinary `zone_setLeader` RPC whenever the zone is ready
to return to the on-chain schedule.

The configured hash must be the local canonical head at startup. After a normal portal transition
ends recovery, the operator must remove the directive before restarting the nodes. Restarting with
the directive after the replacement has produced past `recovery_block_hash` is unsupported.

If a manifest is not specified, `tempo-zone` retains its existing single-sequencer startup
behavior.

## Roles

- The **leader** runs the existing block-production and L1-settlement tasks in
  addition to the P2P network.
- A **follower** joins the P2P network and receives blocks from the leader, validating and importing the blocks and sending a signed block hash back to the leader
- An **rpc-follower** replicates and validates exactly like a follower, but never signs a
  settlement attestation and is not registered with `ZonePortal`. It is a hot standby for public
  RPC: it serves reads from its own imported chain and forwards transactions to the leader, so
  neither the leader nor a quorum follower has to be exposed to the internet. Mark one with
  `rpc_only = true` on its manifest node.

Because rpc-followers are outside the on-chain quorum, they neither raise nor lower the signature
threshold. Quorum membership is the one thing a leadership transition never changes: the portal can
only name a registered sequencer, and an rpc-follower is not one. Promoting a standby is a manual
operation — provision an individual secp256k1 key, register it with `ZonePortal`, drop `rpc_only`
from the manifest, and restart.

Roles are dynamic: a node promotes or demotes at finalized leadership activation
boundaries, driven by the node's role controller. The manifest's
`leader_ed25519_public_key` is only a legacy bootstrap used until the portal reports a
nonzero leader; the (optional) `--sequencer.role` CLI argument is only an assertion checked
against that bootstrap — `leader`, `follower`, or `rpc-follower`. There is no automatic election:
leadership changes only through an operator-triggered `setLeader` transaction finalized on L1.
 

## Commonware network

Nodes use [Commonware](https://commonware.xyz/) to communicate. Discovery is disabled: the
manifest supplies the complete peer set, including each peer's address and
Ed25519 public key.

The Commonware Ed25519 identity answers which configured network peer sent a
message. It is not an on-chain quorum identity. The quorum design will
also give each node an individual secp256k1 key whose Ethereum address is
registered with `ZonePortal`. 

The authenticated-network namespace includes the P2P wire-protocol version,
Tempo L1 chain ID, ZonePortal address, and zone ID. This keeps nodes from different local, test,
or production environments from connecting when a key or endpoint is accidentally reused.

Each node also logs a `membership_digest` covering every member's Ed25519 identity, `rpc_only`
standing, and settlement address. It is diagnostic only — compare it across nodes to spot a
manifest mismatch, whose symptom is settlement stalling because the leader collects signatures
from a different set than it needs. Peer addresses are excluded, so relocating a node does not
change it.

## Manifest example

The manifest is TOML and must contain at least three quorum nodes. The following shows
the configuration shape:

```toml
zone_id = 7
sequencer_set_version = 0
leader_ed25519_public_key = "0xleader..."

[[nodes]]
name = "leader"
ed25519_public_key = "0xleader..."
secp256k1_address = "0x1111111111111111111111111111111111111111"
address = "leader.zone.internal:9200"

[[nodes]]
name = "follower-a"
ed25519_public_key = "0xfa..."
secp256k1_address = "0x2222222222222222222222222222222222222222"
address = "follower-a.zone.internal:9200"

[[nodes]]
name = "follower-b"
ed25519_public_key = "0xfb..."
secp256k1_address = "0x3333333333333333333333333333333333333333"
address = "follower-b.zone.internal:9200"

[[nodes]]
name = "operator-rpc"
ed25519_public_key = "0xrpc..."
address = "operator-rpc.zone.internal:9200"
rpc_only = true
```

`rpc_only` defaults to `false`, so existing manifests keep their current meaning.

`sequencer_set_version` must exactly match the value reported by `ZonePortal`. Version `0` is
valid for the initial sequencer set installed atomically by `ZoneFactory`; later
`setSequencerSet` calls increment it. The field defaults to `1` for compatibility with existing
manifests that omitted it.

To recover a crashed leader, add this top-level table before restarting the fleet:

```toml
[forced_recovery]
leader = "follower-a"
recovery_block_hash = "0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
```

`leader` is a manifest node name and must identify a quorum member. Because the configured hash
must be the current head, the first recovery anchor and portal epoch are taken from the node's
persisted checkpoint; no independently configured height or epoch can disagree with the hash.

An `rpc_only` entry declares no `secp256k1_address` and the node is started without
`--secp256k1.key`. It never signs a settlement attestation, so the key would be dead weight —
and registering such an address with `ZonePortal` would add a signer the zone never collects a
signature from, stalling settlement on a threshold it can no longer reach.

The manifest loader validates that:

- there are at least three quorum nodes (nodes without `rpc_only`);
- the leader is not `rpc_only`;
- `secp256k1_address` is present on every quorum node and absent on every `rpc_only` node;
- node names, Ed25519 public keys, and secp256k1 addresses are unique;
- every address has a non-zero port;
- `leader_ed25519_public_key` identifies one of the nodes;
- the manifest's `zone_id` matches `--zone.id`; and
- both local private keys correspond to the same manifest member.

At startup, once the portal is deployed, the node also checks the manifest against `ZonePortal`
and refuses to start unless every quorum node's `secp256k1_address` is a registered portal sequencer
and `sequencerThreshold()` is nonzero and reachable by the manifest quorum. Both would otherwise
surface as stalled settlement at the next batch boundary.

Registered sequencers the manifest does not list only warn — a demoted standby whose key was never
deregistered holds a share of the threshold nobody signs for, but failing on it would make every
membership change a window in which no node can start.

Apply a `ZonePortal` registration before the manifest edit that adds the node, and the manifest edit
before deregistering.

## Generate a Commonware identity

Generate a unique Ed25519 key for each node with `xtask`:

```bash
cargo run -p tempo-xtask -- generate-p2p-key --out leader-p2p.key
```

The command writes the hex-encoded private key to the requested file and prints
the corresponding public key for the manifest. 

## Start a node with a manifest

Add these arguments to the node's normal command:

```text
--sequencer.manifest ./zone-manifest.toml
--p2p.key ./leader-p2p.key
--secp256k1.key ./leader-secp256k1.key
--p2p.listen 0.0.0.0:9200
--sequencer.role leader
```

Use each node's own key files and listener address. Quorum followers use their individual
secp256k1 keys to sign settlement attestations after importing and validating blocks.
The paths supplied through `--p2p.key`, `--secp256k1.key`, and `--sequencer-key-file` may point
to either regular files or FIFOs.

An rpc-follower is started with **neither** key: it omits `--secp256k1.key` (it never signs an
attestation) and `--sequencer-key`/`--sequencer-key-file` (it never produces a block). The shared
sequencer key is also the zone's ECIES private key for encrypted deposits, so provisioning it on
the internet-facing standby would put deposit recipients and memos within reach of a host
compromise. Startup rejects either flag on an `rpc_only` node rather than ignoring it.
This key is independent from the shared `--sequencer-key`; reusing that shared key
would collapse several nodes into one recoverable quorum identity.
The `--sequencer` flag conflicts with `--sequencer.manifest` because the
manifest determines whether the node starts the sequencer tasks.

DNS peer addresses do not provide a stable egress IP for Commonware's inbound
source-IP filter. A manifest containing any DNS peer therefore requires the
explicit `--p2p.bypass-ip-check` flag. The flag disables source-IP filtering for
all inbound P2P connections, not only the DNS peer. Only use it when a network-level
policy restricts the P2P port to the configured peers; Ed25519 manifest membership
authentication remains enforced.

## Block catch-up

Every node probes for missing blocks when P2P starts and retries while its eligible peers are
offline or a gap remains. Every role can serve bounded 64-block response pages from its persisted
canonical chain.

Catch-up sources are always quorum members — never a standby, so no node takes chain data from an
internet-facing one. Within the quorum, a node asks only the leader while it answers, and widens to
the other quorum members once the leader misses the response timeout. That fallback is what lets an
rpc-follower catch up through a leader outage.

Preferring the leader is a trust boundary, not load balancing. A live block is checked against the
scheduled leader of the anchor it embeds, but a backfilled block has no sender to check — only
parent linkage, L1 anchor, execution and hash. Those pass for a valid *alternative* chain too, so a
compromised quorum member could serve pages that build a fork the quorum will never settle. Asking
the leader first limits that to a leader outage, counted by
`zone_p2p_backfill_requests_without_leader_total`.

Removing it altogether needs the block to carry a leader signature over its own hash, which is a
wire-format change tracked separately.

Backfilled blocks use the same RLP representation and import path as live replicated blocks. A
node buffers out-of-order arrivals, then re-executes and canonicalizes only the next block after
its local head. Parent linkage, execution results, block hash, and forkchoice validation therefore
remain mandatory during catch-up; authenticated transport alone never makes a returned block
canonical.

## Transaction forwarding

Commonware carries blocks, catch-up traffic, and transactions on independent authenticated
channels. A node that is not the leader of the next Tempo anchor it will consume sends canonical
EIP-2718 transaction bytes to every other quorum member. During a scheduled handoff, this lets
both the outgoing and incoming leaders retain transactions before the activation boundary. An
rpc-follower may originate forwarding, but it does not receive forwarded transactions because it
is outside the on-chain quorum.

This permits operator RPC to be exposed on followers while keeping the leader's RPC private. Every
quorum receiver decodes and validates each forwarded transaction through its pool again; follower
validation is not trusted. Only the active leader selects and orders transactions into blocks.
Followers periodically retry live pool transactions, recovering from listener overflow and
temporary leader disconnections.
