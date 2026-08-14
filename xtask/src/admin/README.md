# Admin commands

`tempo-xtask admin` contains operational commands for deployed Tempo Zones. The
commands live in `xtask` because they are one-off CLI operations: they are not
linked into the Zone server and do not run as part of a Zone node.

Currently supported:

- `admin check`: read-only consistency, safety, and liveness checks for a Zone

## `admin check`

`admin check` reads a finalized ZonePortal snapshot from Tempo L1 and compares
it with live data from every configured operator RPC. It does not submit
transactions or change node, Zone, or L1 state.

Build and run it with:

```bash
cargo build -p tempo-xtask
./target/debug/tempo-xtask admin check --config zone-admin.toml
```

### Configuration file

The TOML file contains public operational inputs only. Do not put private keys
or other secrets in it.

```toml
[zone]
id = 1
# Optional. Relative paths are resolved from this config file's directory.
manifest = "./zone-manifest.toml"

[l1]
rpc_url = "https://tempo-rpc.example"
# Optional. The repository's Moderato ZoneFactory is the default.
zone_factory = "0x..."
# Optional. When supplied, this must match the ZoneFactory mapping.
portal = "0x..."

[[nodes]]
name = "leader"
operator_rpc_url = "http://leader.internal:8545"

[[nodes]]
name = "follower-a"
operator_rpc_url = "http://follower-a.internal:8545"

[[nodes]]
name = "rpc"
operator_rpc_url = "http://rpc.internal:8545"
```

Node names are optional, but recommended. An explicit name is checked against
the local manifest name reported by that endpoint.

The expected Zone manifest is also optional. Without it, the command checks
live consistency, including agreement on the loaded manifest Zone ID, version,
and membership digest for multi-node deployments, and clearly reports that the
desired topology was not independently verified. Supplying it additionally
checks those values and every node identity against the expected manifest.

### CLI arguments

Every connection input can be provided without a TOML file:

```bash
./target/debug/tempo-xtask admin check \
  --zone-id 1 \
  --l1-rpc-url https://tempo-rpc.example \
  --portal 0x... \
  --operator-rpc leader=http://leader.internal:8545 \
  --operator-rpc follower-a=http://follower-a.internal:8545 \
  --operator-rpc rpc=http://rpc.internal:8545
```

CLI scalar values override values from `--config`. Supplying one or more
`--operator-rpc` arguments replaces the entire `[[nodes]]` list from the file.

Other useful options are:

```text
--zone-manifest <path>
--observe-for <duration>                    default: 5s
--rpc-timeout <duration>                    default: 10s
--require-sequencer-set-version <version>
--require-leader <node-name-or-address>
--require-encryption-key <x-coordinate>:<parity>
--json
```

Use `--observe-for 0s` for an immediate snapshot. This skips the timed Zone
height check. Durations accept `ms`, `s`, `m`, and `h` suffixes.

### What it checks

The command:

1. Resolves the ZonePortal through ZoneFactory and reads all Portal state at one
   finalized Tempo block.
2. Queries every operator RPC concurrently.
3. Checks Zone and Portal identity, endpoint labels, live quorum membership,
   topology, finalized leader agreement, and promotion readiness on
   non-RPC-only sequencer nodes. RPC-only nodes cannot be promoted and are
   reported as `N/A` for readiness.
4. Verifies the active shared decryption key on non-RPC-only sequencer nodes.
   RPC-only nodes do not hold that key and are reported as `N/A`.
5. Fails when any node's local Zone height is more than 240 blocks (about two
   minutes) behind the newest node, or its Tempo anchor is more than 240 blocks
   behind finalized L1. It then compares block hash and state root across all
   nodes at their lowest common available Zone height.
6. Reports the finalized L1 batch index and settled Zone height. A new L1 batch
   is not required during the short observation window.
7. Samples the operator nodes again after `--observe-for` and verifies that each
   local Zone height advanced.
8. Applies any explicit `--require-*` assertions and, when supplied, compares
   the expected Zone manifest with the Portal and live nodes.
 
