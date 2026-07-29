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

Roles are dynamic: a node promotes or demotes at finalized leadership activation
boundaries, driven by the node's role controller. The manifest's
`leader_ed25519_public_key` is only a legacy bootstrap used until the portal reports a
nonzero leader; the (optional) `--sequencer.role` CLI argument is only an assertion checked
against that bootstrap. There is no automatic election: leadership changes only through an
operator-triggered `setLeader` transaction finalized on L1.
 

## Commonware network

Nodes use [Commonware](https://commonware.xyz/) to communicate. Discovery is disabled: the
manifest supplies the complete peer set, including each peer's address and
Ed25519 public key.

The Commonware Ed25519 identity answers which configured network peer sent a
message. It is not an on-chain quorum identity. The quorum design will
also give each node an individual secp256k1 key whose Ethereum address is
registered with `ZonePortal`. 

The authenticated-network namespace includes the P2P wire-protocol version,
Tempo L1 chain ID, ZonePortal address, and zone ID. This prevents nodes from
different local, test, or production environments from connecting when a key
or endpoint is accidentally reused.

## Manifest example

The manifest is TOML and must contain at least three nodes. The following shows
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
```

The manifest loader validates that:

- there are at least three nodes;
- node names, Ed25519 public keys, and secp256k1 addresses are unique;
- every address has a non-zero port;
- `leader_ed25519_public_key` identifies one of the nodes;
- the manifest's `zone_id` matches `--zone.id`; and
- both local private keys correspond to the same manifest member.

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

Use each node's own key files and listener address. Followers use their individual
secp256k1 keys to sign settlement attestations after importing and validating blocks.
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
offline or a gap remains. Followers request blocks from the statically configured leader. A
recovering leader requests blocks from the configured followers. Both roles can serve bounded
64-block response pages from their persisted canonical chain.

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
members while it holds leadership somewhere in the retained transition schedule.

This permits public RPC to be exposed on followers while keeping the leader's RPC private. The
leader decodes and validates every forwarded transaction again, and it alone selects and orders
transactions for blocks; follower validation is not trusted. Followers periodically retry live
pool transactions, recovering from listener overflow and temporary leader disconnections.
