# Tempo Zone P2P

This crate provides the static networking and the leadership schedule for a
multi-sequencer Tempo Zone. A manifest defines the static peer topology (names, addresses,
Ed25519 identities, individual secp256k1 addresses). Each node loads its own Commonware
Ed25519 identity and validates that it appears in the manifest.

Which member *leads* (produces blocks) is not decided by the manifest: it is derived
exclusively from finalized Tempo L1 state — the `ZonePortal`'s `leader`, `leaderEpoch`, and
`leaderActivationTempoBlock` fields and its `LeaderUpdated` event. Every observed transition
is retained in an activation-indexed `LeadershipSchedule`; production and import authority
for a given Tempo anchor is answered by `leader_for(anchor)` over that retained timeline.

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
Tempo L1 chain ID, ZonePortal address, zone ID, and a digest of the manifest's
settlement membership. The first four prevent nodes from different local, test, or production
environments from connecting when a key or endpoint is accidentally reused.

The membership digest covers each member's Ed25519 identity, whether it is `rpc_only`, and the
individual address its signatures must recover to. Roles are derived locally from each node's own
manifest copy, so peers holding different copies would disagree about who settles — one collecting
signatures from a member the other treats as a non-signing standby. Binding the digest into the
handshake makes that disagreement a failure to authenticate rather than two views of the quorum.
Changing membership therefore requires restarting every node together. Peer *addresses* are
excluded from the digest, so relocating a node stays a rolling operation.

## Manifest example

The manifest is TOML and must contain at least three quorum nodes. The following shows
the configuration shape:

```toml
zone_id = 7
sequencer_set_version = 1
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
name = "public-rpc"
ed25519_public_key = "0xrpc..."
address = "public-rpc.zone.internal:9200"
rpc_only = true
```

`rpc_only` defaults to `false`, so existing manifests keep their current meaning.

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

At startup, before any role task runs, the node also reconciles the manifest against `ZonePortal`
at the finalized head and refuses to start unless:

- `sequencer_set_version` matches the portal's;
- every quorum node's `secp256k1_address` is a registered portal sequencer;
- the portal's registered sequencer count *equals* the manifest quorum count — a registered
  address the manifest does not list holds a share of the threshold nobody signs for, which is
  exactly what a demoted standby leaves behind if its key is not deregistered; and
- `sequencerThreshold()` is nonzero and reachable by the manifest quorum.

A mismatch is a configuration error that would otherwise surface as stalled settlement at the next
batch boundary, so it fails at startup instead.

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

Catch-up sources are always quorum members, and the leader of the next anchor is preferred as the
sole source while it answers. A node widens the request to the rest of the quorum only once the
leader leaves a request unanswered past the response timeout, which is what lets an rpc-follower
keep serving reads through a leader outage — but it never requests from another standby, and no
quorum node ever takes chain data from an internet-facing one.

The preference is a trust boundary, not just a load choice. Live blocks are fenced on the sender
being the scheduled leader of the block's embedded anchor; a backfilled block carries no producer
claim, so it is judged by parent linkage, independent L1 anchor observation, execution, and hash
alone. That rejects garbage, but it cannot distinguish the leader's chain from a valid alternative
built by a compromised quorum follower, so a poisoned page could canonicalize a fork the quorum
will not settle. Preferring the leader confines that exposure to a leader outage.

Closing it entirely needs a producer claim on the block itself — a leader signature over the
sealed block hash, propagated with the block and re-served during backfill — which is a wire-format
change tracked separately. `zone_p2p_backfill_requests_without_leader_total` counts the requests
issued while the exposure is open.

Backfilled blocks use the same RLP representation and import path as live replicated blocks. A
node buffers out-of-order arrivals, then re-executes and canonicalizes only the next block after
its local head. Parent linkage, execution results, block hash, and forkchoice validation therefore
remain mandatory during catch-up; authenticated transport alone never makes a returned block
canonical.

## Transaction forwarding

Commonware carries blocks, catch-up traffic, and transactions on independent authenticated
channels. A follower sends canonical EIP-2718 transaction bytes only to the leader of the next
Tempo anchor it will consume — during a scheduled handoff that remains the outgoing leader
until the activation boundary — and a node accepts transaction messages only from manifest
members while it holds leadership somewhere in the retained transition schedule. An rpc-follower
forwards on exactly this path: public RPC submissions reach the leader without exposing it.

This permits public RPC to be exposed on followers while keeping the leader's RPC private. The
leader decodes and validates every forwarded transaction again, and it alone selects and orders
transactions for blocks; follower validation is not trusted. Followers periodically retry live
pool transactions, recovering from listener overflow and temporary leader disconnections.
