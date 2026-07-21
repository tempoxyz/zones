# Neobank private-Zone benchmark

This is the measured shape for a closed-loop private-Zone journey. It extends
the generic benchmark topology with three distinct assets and five exact
boundaries:

1. encrypted DLUSD onramp and the corresponding `EncryptedDepositProcessed`;
2. a private DLUSD transfer;
3. a composable DLUSD withdrawal to the gateway, a DLUSD/pathUSD swap, vault
   deposit, EarnToken mint, encrypted EarnToken return, and its exact terminal
   deposit event;
4. a composable EarnToken withdrawal to the same gateway, vault redemption,
   optional pathUSD/DLUSD swap, encrypted DLUSD return, and its exact terminal
   deposit event; and
5. a DLUSD off-ramp to the bridge-wallet fixture and its exact
   `WithdrawalProcessed` event.

The fixture contracts are in
`specs/ref-impls/src/benchmark/NeobankFixtures.sol`. `ZoneGatewayFixture`
implements the canonical synchronous `onWithdrawalReceived` callback shape and
the canonical `CallbackData` layout. Its vault and swap are deliberately
narrow, 1:1 test fixtures. They are not a production vault or gateway stack.

## Topology and policy

Provision the existing two-validator L1 plus authenticated private Zone RPC.
The neobank profile must deploy separate DLUSD, pathUSD, and EarnToken fixture
addresses before the Zone is created; DLUSD is its initial token and EarnToken
is enabled after deployment. pathUSD remains L1-only vault collateral.

The profile has zero user bridge and withdrawal protocol fees. It retains a
separate generic profile with nonzero bootstrap fees. The token authorization
map must permit only DLUSD and EarnToken in the Zone. The only callback target
is the gateway fixture and the only terminal off-ramp recipient is the bridge
wallet fixture. No authorization map, mnemonic, private key, encryption
payload, or bearer token belongs in rendered output or an uploaded artifact.

Each composable request uses the exact eight-argument withdrawal overload,
nonzero callback gas, an empty `revealTo`, an account fallback recipient, and a
random action ID used both as the withdrawal memo and callback correlation key.
The terminal matcher requires all of: request transaction hash, sender tag,
queue/deposit hash, action ID, token, recipient, amount, and receipt-scoped
event. Balance polling is not a completion signal.

## Rendered assets

`contrib/bench/neobank/private-flow-scenario.yml` describes the complete
journey. `l1-onramp.yml` contains the encrypted L1 entry; `zone-flow.yml`
contains the private transfer, both composable requests, and the off-ramp.
They are intentionally separate from the generic roundtrip assets.

The transaction generator prepares the onramp payload in memory from the
leased account, action ID, portal address, and current portal encryption key.
It does not write that ciphertext to the scenario report or an artifact. The
secure runtime must still inject the following per-instance values only in
memory:

- encrypted onramp calldata for the leased account and onramp action ID;
- encoded gateway deposit callback data, including its encrypted EarnToken
  return payload, fallback recipient, and action ID; and
- encoded gateway redemption callback data, including its encrypted DLUSD
  return payload, fallback recipient, and action ID.

## Current blocking capability

The pinned transaction generator can now run multi-chain scenarios, exact
receipt-scoped event waits, recipient-aware encrypted onramps, and tuple call
arguments. It cannot yet ABI-encode a tuple into a `bytes` argument, which is
needed for the canonical gateway callback struct.

Consequently, the full command is intentionally not enabled in CI yet. Running
the current scenario would require static callback bytes, which would not
measure the requested flow. The generic roundtrip remains runnable.

The narrow upstream addition needed before enabling this profile is an ABI
encode expression that returns `bytes` from the canonical callback tuple. It
must receive the encrypted return fields and action ID in memory and must not
serialize ciphertext or credentials to the scenario report. Once that lands,
the one-command invocation is:

```bash
contrib/bench/run-neobank-private-flow.sh
```

That runner should provision the existing topology, deploy and configure the
fixtures outside measurement, create the private-RPC authorization map in a
mode-0700 temporary directory, run the scenario, publish per-step and journey
latency, and remove the temporary material on exit.

## Production differences

The topology exercises real L1 and Zone nodes, portal deposits, outbox batches,
authenticated private RPC, and receipt-scoped cross-chain correlation. The
neobank contracts remain minimal fixtures: their 1:1 swap, 1:1 vault conversion,
and unrestricted fixture-token minting do not represent final production
economics, liquidity, policy administration, or vault implementation.
