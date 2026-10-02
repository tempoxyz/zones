# T15 fork-boundary Zone settlement smoke test

The [checked run](evm2-t15-settlement-fork.json) used Tempo
[`3ce628b69`](https://github.com/tempoxyz/tempo/pull/8076) and Zones
`393eb41cc` source, with binary SHA-256 values recorded in the JSON. The Zone
was provisioned before T15 on L1 chain 1337. It received a real encrypted
1,000,000-unit pathUSD deposit in L1 block 72 and credited the private account
by Zone block 50. At that historical point, the account balance, Zone supply,
and L1 portal backing were each 1,000,000.

The dev genesis scheduled T15 for Unix time `1790913144`. L1 block 176 was
pre-fork (`1790913085`); block 177 activated T15 (`1790913223`) after a Tempo
restart on the same datadir. The first post-fork AA `submitBatch` transaction,
[`0x2dd101da…c5ad`](https://github.com/tempoxyz/tempo/pull/8076), settled in
block 182. Its call trace enters the portal's canonical implementation through
a `DELEGATECALL`, which in turn calls the verifier. The block contains only this
batch; the matched payload metric charged all 98,818 gas to payment capacity
and zero gas to general capacity. Its deposit cursor remained 1.

A post-fork native deposit in block 239 used 88,367 gas; its trace has a direct
TIP-20 `transferFrom` child and no portal delegate call. Its isolated block
charged 88,367 payment gas and zero general gas. The Zone credited a second
1,000,000 units by Zone block 163. The next AA batch settled in L1 block 303,
anchored block 239, and advanced the deposit cursor from 1 to 2. Its isolated
block charged 98,794 payment gas and zero general gas. The private account,
Zone supply, and L1 portal backing each measured 2,000,000 after the deposit.

Both batches explicitly used NoProof verifier mode `0x02` with an empty proof.
This demonstrates one existing Zone and its balance crossing the scheduled fork,
plus native deposit and bounded implementation-based settlement with payment
lane attribution. It does not demonstrate cryptographic proof verification,
native withdrawal, vault operations, fresh genesis peer sync, or sustained
combined throughput. The attempted private TIP-20 transfer reverted because
Zone `TIP20Rules` still disables direct transfers; that blocker is tracked in
the [finding ledger](NATIVE_EXECUTION_FINDINGS.md).

Rerun the checker while the isolated devnet and local binaries remain present:

```bash
python3 scripts/native-payments/check-t15-settlement-fork.py \
  --l1-rpc-url http://127.0.0.1:38545 \
  --zone-rpc-url http://127.0.0.1:39545 \
  --portal 0x5ad0000000000000000000000000000000000001 \
  --account 0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266 \
  --pre-deposit-tx 0xff4bc89c1a8e628a33b4324b6818bb5b2953ab222da19679b79bd16c19d043c5 \
  --post-deposit-tx 0x770feeb0bbdef79bb442f4ed24c60aab61b32c19a687b2333998bf12cc4d9bce \
  --first-batch-tx 0x2dd101da491fa9f55f90a83fc151f7b32ea94e80b25e45f744a4cdbdfa21c5ad \
  --second-batch-tx 0x19051cdcb4a1a2310c629ad09f2bf82a329096df5e9f21b92b3ff6092c4c9419 \
  --fork-time 1790913144 --pre-zone-block 50 --post-zone-block 163 \
  --metrics /tmp/evm2-t15-settlement-lane-metrics.jsonl \
            /tmp/evm2-t15-settlement-lane-metrics-late.jsonl \
  --tempo-binary /tmp/native-payments-tempo/target/release/tempo \
  --zone-binary target/release/tempo-zone \
  --output /tmp/evm2-t15-settlement-fork-recheck.json
```
