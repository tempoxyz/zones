
<br>

<p align="center">
  <a href="https://tempo.xyz/blog/introducing-tempo-zones">
    <img src="assets/header.png" alt="Tempo Zones" width="100%">
  </a>
</p>

# Zones

> [!NOTE]
> This repository is actively under development and subject to rapid iteration.
> APIs, interfaces, and behavior may change without notice. Not recommended for production use yet.

Zones are private blockchains anchored to [Tempo](https://github.com/tempoxyz/tempo) *(currently available in testnet only),* with native support for confidential balances and transactions. Zones inherit compliance via TIP403 policies from Tempo and support interoperability with Tempo for moving assets in and out of Zones.

You can get started today by [deploying a Zone](#getting-started) on Tempo testnet, reading the [Zones documentation](https://docs.tempo.xyz/guide/private-zones), or exploring the [Zone spec](docs/specs/zone_spec.md).

<br>

## What Makes Zones Interesting

- **Private balances and transactions.** State access requires account authentication at the RPC layer. This ensures that only the authorized account holder can access balances and transaction history. The Zone operator maintains full visibility into state for compliance.

- **Encrypted deposits and withdrawals.** When depositing into a Zone, users can encrypt the recipient to not reveal who receives funds inside the Zone. Encrypted withdrawals are also possible, allowing the sender to be replaced with a commitment, preserving recipient verifiability without exposing the sender when withdrawing to Tempo mainnet.

- **Zone to Zone transfers.** Zones interoperate with Tempo via withdrawals with optional calldata. Withdrawal calldata can execute on Tempo and deposit into another Zone, enabling flows like Zone to Zone transfers or executing a swap between sending amounts to another Zone.

- **Compliance inherited from Tempo.** [TIP-403](https://docs.tempo.xyz/protocol/tip403/overview) policies (whitelist, blacklist) are mirrored from Tempo and enforced on Zones. Issuers set the policy once on Tempo and the Zone picks it up automatically. If an issuer freezes an address or updates a blacklist on Tempo, the Zone inherits the change in the next block in the Zone.

- **Fast withdrawals.** The Zone processes transactions every 250ms and submits batches of withdrawals to Tempo, where blocks are produced every ~500ms. Once batches are accepted and the attached proof is validated, withdrawals are processed and funds are released from escrow.

<br>

## Getting Started

Prerequisites: [Rust](https://rustup.rs/), [Foundry](https://book.getfoundry.sh/getting-started/installation), [`just`](https://github.com/casey/just#packages), [`jq`](https://jqlang.github.io/jq/download/)


### Deploying a Zone

```bash
# Deploy and start a zone on Moderato testnet
export L1_RPC_URL="wss://rpc.moderato.tempo.xyz"
just deploy-zone my-zone
```

The `deploy-zone` command generates a sequencer keypair, funds it on L1, deploys the portal via `ZoneFactory`, generates genesis, and starts the node.

```bash
# Start/restart a zone after initial deployment
just zone-up my-zone
```

### Depositing into a Zone

```bash
export L1_PORTAL_ADDRESS=$(jq -r '.portal' generated/my-zone/zone.json)
just max-approve-portal

# deposit into the zone
just send-deposit 1000000                       # deposit to your own address
just send-deposit 1000000 <recipient-address>   # deposit to a specific address
```

```bash
# send an encrypted deposit
just send-deposit-encrypted 1000000                       # to your own address
just send-deposit-encrypted 1000000 <recipient-address>   # to a specific address
```

### Withdrawing from Zone to Tempo

```bash

# withdraw from the zone
just max-approve-outbox
just send-withdrawal 1000000 <recipient-address>  # withdraw to a specific address
```

The sequencer includes the withdrawal in the next batch submission to L1 and processes it automatically.


### Querying the Private RPC

Zone balances are private by default. Every RPC request must include a signed authorization token that proves you control the querying account.

`just zone-auth-token` reads `generated/<name>/zone.json` and signs a short-lived auth token:

```bash

# generate an auth token
export PRIVATE_KEY=<zone-wallet-private-key>
export TOKEN=$(just zone-auth-token my-zone)

# query your TIP-20 balance 
just check-balance-private my-zone <token-address>
```


See [docs/ZONES.md](docs/ZONES.md) for the full guide on deposits, withdrawals, private RPC, router demos, TIP-403 policy flows, and command references.

<br> 

## License

Licensed under either of [Apache License](./LICENSE-APACHE), Version
2.0 or [MIT License](./LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in these crates by you, as defined in the Apache-2.0 license,
shall be dual licensed as above, without any additional terms or conditions.
