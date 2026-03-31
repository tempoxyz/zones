# Tempo Zones

Zones are sidechains anchored to Tempo L1. Each zone has its own sequencer, genesis state, and portal contract on L1 that escrows deposits and processes withdrawals.

**Explorers:** [Moderato](https://explore.moderato.tempo.xyz/) · [Devnet](https://explore.devnet.tempo.xyz/)

This repository contains the `tempo-zone` node, zone-specific precompiles and RPC support, and the `just` workflows for deploying and operating zones on Tempo L1.

## Quick Start

Prerequisites:

- [Rust toolchain](https://rustup.rs/)
- [Foundry](https://book.getfoundry.sh/getting-started/installation) (`cast`, `forge`)
- [`just`](https://github.com/casey/just#packages)
- [`jq`](https://jqlang.github.io/jq/download/)

Deploy and start a zone on Moderato:

```bash
export L1_RPC_URL="wss://eng:bold-raman-silly-torvalds@rpc.moderato.tempo.xyz"
just deploy-zone my-zone
```

To choose a different initial TIP-20 on the portal at deploy time, pass it as the second positional argument:

```bash
just deploy-zone my-zone alphausd
```

`just deploy-zone` will:

- Generate a fresh sequencer keypair
- Fund the sequencer on L1 via `tempo_fundAddress`
- Build the Solidity specs
- Deploy a zone on L1 via `ZoneFactory`
- Generate `generated/<name>/genesis.json` and `generated/<name>/zone.json`
- Register the sequencer encryption key and start the zone node

`zone.json` stores the deployed portal address, zone ID, anchor block, and sequencer metadata used by later commands such as `just zone-up` and `just deploy-router`.

To restart the same zone later:

```bash
just zone-up my-zone false release
```

## How Zones Work

- A zone sequencer subscribes to Tempo L1 for headers, deposits, and token-enablement events, including backfill from the zone's anchor block.
- The zone builds one sidechain block per L1 block, processing L1-driven state transitions through system transactions before app transactions.
- The zone monitor batches zone blocks back to L1 and processes withdrawals from the zone back to L1 users.

## More Docs

See [docs/ZONES.md](docs/ZONES.md) for:

- Step-by-step setup and deployment
- Deposits, withdrawals, and private RPC usage
- Router demos and TIP-403 policy flows
- Architecture, configuration, and command reference

## Development

```bash
git clone https://github.com/tempoxyz/zones.git
cd zones
cargo build --bin tempo-zone
cargo test --workspace
```

The main binary in this repository is `tempo-zone`:

```bash
cargo run --bin tempo-zone -- node --help
```

## Contributing

Our contributor guidelines can be found in [`CONTRIBUTING.md`](https://github.com/tempoxyz/tempo?tab=contributing-ov-file).

## Security

See [`SECURITY.md`](https://github.com/tempoxyz/tempo?tab=security-ov-file). Note: Tempo is still undergoing audit and does not have an active bug bounty. Submissions will not be eligible for a bounty until audits have concluded.

## License

Licensed under either of [Apache License](./LICENSE-APACHE), Version
2.0 or [MIT License](./LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in these crates by you, as defined in the Apache-2.0 license,
shall be dual licensed as above, without any additional terms or conditions.
