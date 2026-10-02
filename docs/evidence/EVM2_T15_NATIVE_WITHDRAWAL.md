# T15 native withdrawal smoke test

The [checked run](evm2-t15-native-withdrawal.json) uses Tempo source
`e96f3f3bb` and Zones source `afbabc891`; the binary hashes, clients, and
genesis hashes are in the JSON. The Zone 3 account had 600,000 private pathUSD
after the [private-transfer run](EVM2_T15_PRIVATE_TRANSFER.md). It approved the
Zone outbox and signed a 100,000-unit `requestWithdrawal` in L2 block 2430.
The Zone burned the amount. At that block, its three account balances were
500,000, 350,000, and 50,000, summing to its 900,000 supply.

The sequencer submitted AA `processWithdrawals` transaction
`0xa4e3fc4cb850cbf71b7604dfcb75680934cf18dab089b6dafacbe511f8735e1b`
in L1 block 5190. The T15 native entry verified portal registration, proxy
code, sequencer authority, and bounded canonical calldata, then entered the
canonical implementation through a paid EVM2 delegate frame. Its trace shows
a TIP-20 transfer child. The portal's FIFO head advanced from 1 to 2; portal
backing fell from 1,000,000 to 900,000 and the public recipient gained exactly
100,000. The block contained only this transaction. Its matched payload metric
reported **97,468 payment gas and zero general gas**.

The same calldata sent to unregistered portal-prefix address Zone 4 reverted
at L1 block 5260 without any child call. Its isolated block charged 27,910
general gas and zero payment gas; the real portal's backing stayed 900,000.
This checks that a forged destination cannot gain payment capacity from its
selector and prefix. The actual payout was a plain transfer; callback and
failure bounce paths still need composed devnet tests. Settlement used explicit
NoProof mode, so this run does not establish cryptographic proof validity.

Rerun the checker while the isolated devnet is available:

```bash
python3 scripts/native-payments/check-t15-native-withdrawal.py \
  --output /tmp/evm2-t15-native-withdrawal-recheck.json
```
