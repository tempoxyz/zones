# Experimental QMDB Zone prover

This draft adds an opt-in Commonware **Current, ordered, variable-value QMDB**
backend to the Zone stateless proof function. It does not switch a running Zone
node or settlement to QMDB. MPT remains the default; the prover request has an
optional experimental QMDB history field.
The QMDB implementation matches the workspace's Commonware `2026.9.0` version.

For an isolated runnable single-node Zone using the real executor, QMDB roots,
signed transactions, RPC, restart and proof replay, see [QMDB-TESTNET.md](QMDB-TESTNET.md).
That runner uses a mock Tempo L1 and a full-history journal, not the production
node's storage/provider integration.

## Execution and commitments

`zone-spf/qmdb` exposes `prove_qmdb_zone_batch`. It uses the same transaction replay,
system calls, checkpoint checks, block assembly and public outputs as MPT replay.
Only Zone state reads and post-state commitments select the QMDB backend. Tempo
headers and Tempo storage proofs remain authenticated by their existing MPT roots.

The QMDB parent header must contain a **trusted QMDB pre-state root**. Existing
MPT parent headers cannot be passed unchanged. The resulting Zone headers have
different hashes, so successors and settlement commitments must also be rebuilt.
This API is an ordinary Rust SPF, not a new zk circuit or a deployed Nitro image.
The enclave service can opt in at build time with its `qmdb` feature. An explicit
`qmdbStateWitness` request selects QMDB replay; builds without support reject it
instead of silently using MPT. MPT requests omit the field and preserve the prior
three-field CBOR encoding. The normal sequencer continues sending MPT requests.

The initial witness contains the complete unpruned semantic mutation history,
preserving batch boundaries. Its first batch imports a checkpoint; subsequent
batches correspond to blocks. Each batch is sorted by `QmdbKey` and has unique
keys. The prover rebuilds QMDB from that history to authenticate the parent root,
then appends each executed block's changes. It currently reconstructs the full
history for each root calculation. This is a correctness prototype, not a
constant-time stateless update proof or a persistent-node performance measurement.

Account and storage keys are domain-separated. Keys retain the keccak-hashed
address and, for storage, the keccak-hashed 32-byte slot. Account values encode
nonce, balance and code hash as an RLP `TrieAccount` with the storage-root field
fixed to Ethereum's empty trie root; storage is committed separately as flat
QMDB rows. Storage values are nonzero 32-byte big-endian words. Zero writes delete
rows. Destruction and storage resets remove all old slots before applying new
writes. Bytecode still authenticates against its account code hash.

Commitments use Commonware's Current root, including the activity bitmap, not
the raw operations root. `read_proof` and `verify_read_proof` generate and verify
Current inclusion and ordered exclusion proofs against an externally trusted root.
These read proofs are not yet sufficient to replace the full-history **transition**
witness: authenticating reads alone does not prove that a supplied new root is the
correct update of the old one.

## Conversion limits

`import_mpt_snapshot` accepts a root-bound **complete** account and storage trie.
It traverses hashed and inline children, requires every referenced node, imports
every account and nonzero slot, and emits a deterministic initial QMDB batch.
Hashed key preimages are unnecessary for the import because the flat key scheme
uses those same hashed components. Execution still hashes actual addresses and
slots to resolve reads.

A recent execution witness normally contains only accessed paths. Hash references
for untouched state do not reveal the missing key/value data; importing those
paths as a complete database would fabricate absence for untouched accounts.
The converter rejects such snapshots with `IncompleteMpt` instead. A safe migration
needs a full checkpoint export plus authenticated execution data after that checkpoint.

Existing MPT Merkle paths, Nitro attestations and public block-hash commitments
cannot be relabeled as QMDB proofs. Generate new QMDB read proofs from the imported
checkpoint, replay blocks using the QMDB predecessor headers, and obtain new
attestations under a reviewed QMDB prover policy. This draft does not provide that
deployment or migrate historical settlement commitments.

## Run

```sh
cargo test -p zone-spf --features qmdb
cargo run -p zone-spf --features qmdb --example qmdb-data -- --demo
```

The demo explicitly labels its synthetic input: 256 accounts with eight storage
slots each. It emits the Current root, full-history JSON size, encoded inclusion
and exclusion proof sizes, and verification results. Its wall-clock root time
includes reconstruction and is not a production benchmark or an MPT comparison.
The measured development-build run in `qmdb-synthetic-sample.json` has 2,304 rows,
561,933 bytes of full-history JSON, 519-byte inclusion proofs and 584–585-byte
exclusion proofs. All four sampled proofs verified. These sizes do not include
the separate inclusion value, framing, attestation or an authenticated update proof.

For an externally authenticated complete MPT snapshot, supply JSON with
`stateRoot`, `state` (the hex RLP nodes) and `readKeys` (hashed account/slot pairs):

```sh
cargo run -p zone-spf --features qmdb --example qmdb-data -- snapshot.json
```

For local SPF replay against an already-QMDB parent header:

```sh
cargo run -p zone-spf --features qmdb --example qmdb-data -- \
  --prove-qmdb genesis.json batch.json history.json
```

`batch.json` is the existing `BatchWitness` schema, with an empty Zone MPT node
pool and the necessary bytecode preimages. `history.json` is `QmdbStateWitness`.
The supplied genesis is verifier-selected configuration, not prover-controlled
configuration. Do not patch an old MPT batch's roots and call that a verified replay.

For the existing attested service transport, build the enclave binary with
`--features qmdb`, approve its new measurements in the normal attestation policy,
and use the saved QMDB-rooted batch plus complete history:

```sh
cargo run -p tempo-zone-prover-utils -- prove \
  --input batch.json --qmdb-history history.json \
  --target "$PROVER_TARGET" --attestation-policy qmdb-measurements.json \
  --output qmdb-proof.json
```

Only the replay/dispatch and wire paths are tested locally. No live Nitro image,
NSM attestation or on-chain QMDB settlement was exercised for this draft.

## Live baseline and access gap

`qmdb-live-sample.json` records live devnet `tempo-zone-prover` MPT prover telemetry
retrieved on October 5, 2026 from `dev-eu-vl-internal`. Eight sequential batches
cover blocks 1,234,321–1,235,280: 960 blocks total. Each batch has 120 blocks,
746 Zone trie nodes, 131–137 Tempo trie nodes, 273,223–277,914 recorded witness
bytes and 60–67 ms recorded prover elapsed time. All eight have zero user
transactions, deposits and withdrawals; they are not workload throughput evidence.
Three transaction-bearing single-block batches from the same day contain 2–3
transactions, 73,745–95,020 recorded witness bytes and 20–52 ms elapsed time.
The recorded `witnessBytes` is the sequencer metric, not a downloaded JSON size.

Raw execution witnesses were **not** downloaded. The Cloudflare RPC endpoint
returned HTTP 401 (missing username/password). Kubernetes reads succeeded but
`pods/portforward` was denied for the sandbox principal. No authentication bypass,
credential change or production deployment was performed. An authorized RPC
connection or attached witness export is needed for real-witness conversion and
replay; a complete checkpoint is additionally required for safe root migration.

## Remaining node work

Before a Zone can actually run QMDB instead of MPT, integrate a persistent database
with payload construction, execution validation, genesis, historical state queries,
reorg/rewind, snapshot/state sync, RPC proof schemas and witness generation. Specify
the activation checkpoint, finalize the QMDB witness/attestation policy and update
node consensus together. Replace full-history reconstruction with an authenticated update witness,
then compare live workloads with MPT using matching hardware and checkpoint data.
The draft deliberately leaves those paths unchanged rather than producing QMDB
headers from a node whose verifier and historical state remain MPT-only.
