# Runnable single-node QMDB test Zone

This branch includes `tempo-zone-qmdb`, an isolated local Zone runner using the
real Zone executor and native precompiles, Commonware Current QMDB state roots,
signed transaction execution, Ethereum-style HTTP RPC, durable state, rewind,
and native SPF proof replay. Zone state does not use an MPT. Mock **Tempo L1**
state remains an empty MPT, as in the normal Zone prover's L1 interface.

This is a testing version, not a replacement for the production `tempo-zone`
node. It is single-node, uses a deterministic empty mock L1, and has no live
portal, encrypted bridge ingress, P2P, remote state sync, Nitro attestation or
on-chain settlement. It cannot follow a real Tempo chain. The normal MPT node
and sequencer remain unchanged.
The runner only constructs empty L1 imports and zero-withdrawal batches; this
version does not provide bridge deposit/withdrawal testing.

## Run

From the repository root, with the usual Rust/native build prerequisites:

```sh
cargo run --locked -p tempo-zone-qmdb -- \
  --datadir ./qmdb-zone-data \
  --http 127.0.0.1:9545 \
  --block-time 1
```

The RPC is intentionally restricted to loopback. It has no authentication and
exposes test-only mining and rewind methods; do not forward it publicly.
On restart, reuse the same datadir. A second process cannot open a locked datadir.
To create a fresh chain, use another directory, not an existing MPT node datadir.

Empty blocks are mined every second, and each submitted valid transaction is
mined immediately. Use `--block-time 0` for manual/transaction-driven mining.
Block timestamps advance deterministically by one second, rather than tracking
wall time. Each block executes `advanceTempo`, user transactions, and zero-count
withdrawal finalization, then commits the QMDB post-state root in its header.

The standard public Anvil mnemonic's first account is funded with 1,000,000
pathUSD (1,000,000,000,000 base units) for **Zone fees**, not native ETH:

```text
test test test test test test test test test test test junk
0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266
```

Never use this publicly known mnemonic for real funds. Chain ID is returned by
`eth_chainId`; it is derived from parent chain 1337 and Zone ID 1.

## Exercise the node

Foundry's `cast` and `jq` are used only for these examples:

```sh
cast rpc --rpc-url http://127.0.0.1:9545 qmdb_status
cast rpc --rpc-url http://127.0.0.1:9545 evm_mine
cast rpc --rpc-url http://127.0.0.1:9545 qmdb_proveBlock '"0x1"'
```

The proof result explicitly reports `attested: false` and
`kind: native-spf-replay`; it is a verified native execution result, not a
cryptographic attestation or a zk proof.

For signed calls, use the existing Zone-native TIP-20 precompiles. Ordinary
`CREATE` transactions and EIP-7702 authorizations remain prohibited by the
existing Zone transaction policy; this runner does not loosen it or increase
the Zone's non-payment gas budget to enable arbitrary EVM contracts.
Public TIP-20 transfers are also disabled by current Zone policy; approvals
provide a permitted state-changing transaction for this test.

```sh
cast send --rpc-url http://127.0.0.1:9545 \
  --mnemonic 'test test test test test test test test test test test junk' \
  --legacy --gas-limit 500000 --gas-price 10000000000 \
  0x20c0000000000000000000000000000000000000 \
  'approve(address,uint256)' 0x000000000000000000000000000000000000b001 42

cast call --rpc-url http://127.0.0.1:9545 \
  --from 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266 \
  0x20c0000000000000000000000000000000000000 \
  'allowance(address,address)(uint256)' \
  0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266 \
  0x000000000000000000000000000000000000b001
```

The last command returns an allowance of 42 base units. Restarting the node retains
it. Native proof replay of that block reconstructs the identical header hash.

Supported RPCs include chain/head queries, blocks, transactions, receipts,
current balances/nonces/code/storage, `eth_sendRawTransaction`, `eth_call`, and
best-effort `eth_estimateGas` (measured simulation usage plus a fixed margin).
Historical state queries, state/block overrides, subscriptions, logs filtering,
wallet-managed `eth_sendTransaction`, and Ethereum MPT `eth_getProof` are not
implemented; unsupported methods fail rather than returning fabricated results.
`safe` and `finalized` aliases mean the local head, not L1-finalized settlement.

## Witnesses and read proofs

```sh
cast rpc --rpc-url http://127.0.0.1:9545 \
  debug_qmdbExecutionWitness '"0x1"' > qmdb-proof-input.json
cast rpc --rpc-url http://127.0.0.1:9545 \
  qmdb_exportCheckpoint > qmdb-checkpoint.json
cast rpc --rpc-url http://127.0.0.1:9545 \
  qmdb_getProof '"0x000000000000000000000000000000000000b001"' '["0x0"]'
```

`debug_qmdbExecutionWitness` exports the existing `BatchWitness` plus the exact
QMDB pre-state history. The parent header is QMDB-rooted; it is not an MPT witness
with its root relabeled. The checkpoint includes network genesis, head header,
current history and bytecodes. Independently select and trust the network's
genesis/checkpoint before accepting a remote proof.

`qmdb_getProof` returns the Current root, account key/proof, and requested storage
keys/proofs, with up to 32 slots per request. Verify them with
`zone_spf::qmdb::verify_read_proof`; read proofs alone do not authenticate updates.

For the local example verifier, split the exported input:

```sh
jq '.genesis' qmdb-checkpoint.json > genesis.json
jq '.witness' qmdb-proof-input.json > batch.json
jq '.qmdbStateWitness' qmdb-proof-input.json > history.json
cargo run --locked -p zone-spf --features qmdb --example qmdb-data -- \
  --prove-qmdb genesis.json batch.json history.json
```

The optional QMDB-enabled enclave transport is described in [QMDB.md](QMDB.md);
this local runner does not provision a Nitro image or approve its measurements.

## Persistence and limits

`chain.json` stores the semantic QMDB mutation history, authenticated bytecode
preimages, blocks, receipts and original replay inputs. The history is stored
once, and block witnesses select its prefix. Commits use file sync, atomic rename
and directory sync, under an exclusive datadir lock. On startup, execution from
the fixed test genesis must reproduce every stored block and final state.
Invalid transactions or failed replay do not advance the in-memory head/journal.

This is **not** a persistent Commonware disk-backed database implementation: it
persists a replay journal and reconstructs the in-memory QMDB. Root construction
and state RPCs rebuild the full history; startup replay and journal rewrites get
more expensive as the chain grows. This is suitable for small functional tests,
not long-running load tests or the compact-witness performance estimates.

`qmdb_rewindTo` explicitly rewinds the test chain, re-executes its retained prefix
and saves it atomically; abandoned blocks are removed from this journal. It is
not a production reorg/state-sync implementation. Save a checkpoint first if
you need the abandoned test branch.

```sh
cargo test --locked -p tempo-zone-qmdb
cargo test --locked -p zone-spf --features qmdb
```

Tests cover real signed token approvals, fee-funded sender nonces, persisted code
and storage, invalid transaction atomicity, native proof parity, restart,
rewind/replay, datadir locking, corrupted journal rejection and RPC dispatch.

[qmdb-testnet-smoke.json](qmdb-testnet-smoke.json) records the HTTP smoke test:
two signed approvals, successful receipts, allowance retained across process
restart, nonce 2, and matching native proof/block hashes. This is functional
test data from the mock-L1 runner, not live Zone witness conversion or a benchmark.
