# T15 Zone private transfer and settlement smoke test

The [checked record](evm2-t15-private-transfer.json) uses Tempo source
`3ce628b69` and Zones source `afbabc891`; its binary hashes, client versions,
and genesis hashes are in the JSON. A new Zone 3 was provisioned against the
post-T15 Tempo devnet with `--dev.t15-time 1790913144`. Its saved genesis and
startup hardfork list both contain T15 at that timestamp. This establishes
Zone execution under T15; the Zone was created after L1 activation, so this
run does not establish pre-fork Zone-state migration.

Replaying an encrypted deposit prepared for Zone 1 into Zone 3 produced a
valid L1 queue transaction but failed Zone decryption. The Zone emitted a
refund request; portal backing was zero by L1 block 2899 and the private
account had not been credited. This is an expected portal-bound encryption
failure, observed through the actual refund path.

A fresh encrypted 1,000,000-unit deposit used Zone 3's public encryption key,
key index, and portal address. Its L1 transaction
`0xd89be43645e2fdd5066ac9fd95bab7e9411e52ffac9f10272d380f23777a9dfe`
was included at block 2901; a native TIP-20 child call is visible in its
trace. The Zone credited exactly 1,000,000 at L2 block 268. Three signed
private TIP-20 transfers then succeeded at L2 blocks 281, 350, and 448. At
block 448, the sender held 600,000, the first recipient 350,000, and a new
second recipient 50,000. Zone supply and L1 portal backing were each
1,000,000. The 250,000-gas difference between the first and second sender
transactions coincided with sender nonce 0→1; a later transfer to another new
recipient had the same steady-state gas as the second transaction. This
observed difference is therefore consistent with first-sender account state
gas, not recipient novelty.

AA batch `0x199e855ed2dafb7c72e6dea8a9162ee48530ac1925e60228f14b1c72a9443c8b`
settled Zone height 350, covering the second transfer, in isolated L1 block
3047. Its matched payload metric charged 98,842 payment gas and **zero**
general gas. The batch used explicit NoProof mode `0x02` with an empty proof.
After this run, the Zone process was stopped and restarted with `tempo-zone
node` using the saved genesis and datadir; the same genesis hash and all three
balances persisted. This is a stateful restart check, not fresh peer sync
from genesis or a cryptographic proof test.

The calldata generator can reproduce a portal-bound deposit:

```bash
cargo run --release -p zone-precompiles --example encrypt_deposit -- \
  PORTAL SENDER RECIPIENT TOKEN AMOUNT KEY_X KEY_PARITY KEY_INDEX
```

Rerun the live checker while this devnet is available:

```bash
python3 scripts/native-payments/check-t15-private-transfer.py \
  --output /tmp/evm2-t15-private-transfer-recheck.json
```

This run verifies a native deposit, private movement, custody conservation,
restart, and one payment-lane settlement. Native withdrawal, vault lifecycle,
real execution proofs, pre-fork Zone migration, and sustained combined
Earn/Zone throughput remain open.
