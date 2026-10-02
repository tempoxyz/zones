# EVM2 bridge baseline — 2026-10-01

This is a real transaction baseline on local Tempo and Zone nodes. It does not
activate native portals/vaults, prove Zone execution, certify migration, measure
sustained throughput, or establish zero general-lane payment usage. Settlement
uses the explicit temporary `NoProof` verifier configuration `0x02`.

Exact node revisions, binary hashes, genesis hashes and five successful canonical
receipts are recorded in [evm2-bridge-baseline.json](evm2-bridge-baseline.json).
A public transfer delivered 1,000,000 pathUSD units. An ECIES-encrypted deposit
then delivered 1,000,000 units to the Zone; its authenticated private balance was
1,000,000 after restarting the existing node database. A subsequent 200,000-unit
withdrawal settled and paid the public recipient. Final private supply, private
account balance and portal backing were each 800,000; the public recipient held
1,200,000 units.

## Integration findings

1. Tempo's EVM2 branch omitted the millisecond future-timestamp checks and the
   configuration method required by Zones. Restoring the upstream implementation
   passed all 22 consensus tests.
2. The dev genesis contains a pre-existing pathUSD whose transfer-policy ID had
   not been migrated to TIP-403. `createZone` correctly rejected it with
   `TokenTransferPolicyNotSet`. Calling `migrateTransferPolicyIds([pathUSD])`
   initialized the policy registration before provisioning.
3. Unconfigured Zones settlement selected Nitro mode with an empty proof. Tempo
   correctly rejected it with `InvalidProof`. The corrected unconfigured path
   selects `NoProof` explicitly. A supplied invalid or empty Nitro proof remains
   an error; a configured prover failure cannot reach the unconfigured branch.
4. The latest Tempo typed dispatch uses `view` and `mutate`, and its T12+ static
   mutations halt. Zone dispatch now follows that API; tests cover T11, T12 and
   T13. Newly added `burnAt` remains an unauthorized Zone administrative call.

These fixes establish a usable comparison baseline. They do not satisfy the
native-payment delivery gates in [the delivery contract](../EVM2_NATIVE_PAYMENTS.md).

## Recheck a running baseline

`scripts/native-payments/check-baseline.py` reads the actual receipts, verifies
canonical block hashes, checks the settlement ABI's NoProof mode and empty proof,
requires the portal-to-recipient payout log, and checks private supply against
portal backing. It exits nonzero on a failed assertion. For this saved run:

```sh
python3 scripts/native-payments/check-baseline.py \
  --l1-rpc-url http://127.0.0.1:18545 --zone-rpc-url http://127.0.0.1:19545 \
  --portal 0x5ad0000000000000000000000000000000000001 \
  --account 0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266 \
  --public-recipient 0x000000000000000000000000000000000000beef \
  --zone-balance 800000 --public-recipient-balance 1200000 --withdrawal-amount 200000 \
  --transfer-tx 0x17248d928843922fa9a9f3228c8168fed8ca1853d9193fc330371a306109f7e4 \
  --deposit-tx 0xd023b14043f88b44e3dd988eeba7de0ab631c29aca5b68caa25a642bd890e56a \
  --withdrawal-tx 0xdd87321e41ab03139db5ef2c9fc6e27411d7b9ef082437c52068657602aa643b \
  --settlement-tx 0xc9864541776658fa67b32b1729d588bcb4f6ded8f4af35ceda4951ab1f73a5ab \
  --finalization-tx 0x92383579fec14e75f14c57c40af1ad490fc999af8b9a8d2f064a773ef8b7422d \
  --output /tmp/evm2-bridge-baseline-recheck.json
```

The initial L1 used Tempo's `crates/chainspec/src/genesis/dev.json`, isolated
ports 18545/18546 and an isolated datadir. The Zone was provisioned with
`tempo-zone dev`, ports 19545/19546/19547 and private RPC 18544, then restarted
with `tempo-zone node` against its saved genesis, sequencer key and database.
`tempo-xtask deposit` encrypted the actual deposit. `cast send` submitted the
TIP-20 approvals and native Zone outbox withdrawal request.

No general-lane attribution was measured in this baseline. The scheduled upgrade
runner and real execution-proof settlement remain required deliverables.
