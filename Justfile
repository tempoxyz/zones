cross_compile := "false"
cargo_build_binary := if cross_compile == "true" { "cross" } else { "cargo" }
act_debug_mode := env("ACT", "false")

[group('deps')]
install-cross:
    cargo install cross --git https://github.com/cross-rs/cross

[group('build')]
[doc('Builds all tempo binaries in cargo release mode')]
build-all-release extra_args="": (build-release "tempo" extra_args)

[group('build')]
[doc('Builds all tempo binaries')]
build-all extra_args="": (build "tempo" extra_args)

build-release binary extra_args="": (build binary "-r " + extra_args)

build binary extra_args="":
    {{cargo_build_binary}} build {{extra_args}} --bin {{binary}}

[group('localnet')]
[doc('Generates a genesis file')]
genesis accounts="1000" output="./" profile="maxperf":
    cargo run -p tempo-xtask --profile {{profile}} -- generate-genesis --output {{output}} -a {{accounts}} --no-dkg-in-genesis

[group('localnet')]
[doc('Deletes local network data and launches a new localnet')]
[confirm('This will wipe your data directory (unless you have reset=false) - please confirm before proceeding (y/n):')]
localnet accounts="1000" reset="true" profile="maxperf" features="asm-keccak" args="":
    #!/bin/bash
    if [[ "{{reset}}" = "true" ]]; then
        rm -r ./localnet/ || true
        mkdir ./localnet/
        just genesis {{accounts}} ./localnet {{profile}}
    fi;
    cargo run --bin tempo --profile {{profile}} --features {{features}} -- \
                      node \
                      --chain ./localnet/genesis.json \
                      --dev \
                      --dev.block-time 1sec \
                      --datadir ./localnet/reth \
                      --http \
                      --http.addr 0.0.0.0 \
                      --http.port 8545 \
                      --http.api all \
                      --engine.disable-precompile-cache \
                      --engine.legacy-state-root \
                      --builder.gaslimit 3000000000 \
                      --builder.max-tasks 8 \
                      --builder.deadline 3 \
                      --log.file.directory ./localnet/logs \
                      --faucet.enabled \
                      --faucet.private-key 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 \
                      --faucet.amount 1000000000000 \
                      --faucet.address 0x20c0000000000000000000000000000000000001 \
                      {{args}}

[group('zone')]
[doc('Approves the ZonePortal to spend max TEMPO. Requires L1_RPC_URL and PRIVATE_KEY env vars.')]
max-approve-portal:
    #!/bin/bash
    set -euo pipefail
    RPC="${L1_RPC_URL:?Set L1_RPC_URL env var}"
    PK="${PRIVATE_KEY:?Set PRIVATE_KEY env var}"
    PORTAL="0x1bc99e6a8c4689f1884527152ba542f012316149"
    TOKEN="0x20C0000000000000000000000000000000000000"
    HTTP_RPC=$(echo "$RPC" | sed 's|^wss://|https://|' | sed 's|^ws://|http://|')
    echo "Approving ZonePortal for max TEMPO..."
    cast send "$TOKEN" "approve(address,uint256)" "$PORTAL" "$(cast max-uint)" \
        --rpc-url "$HTTP_RPC" --private-key "$PK"
    echo "Approved!"

[group('zone')]
[doc('Sends a test deposit to the ZonePortal on L1 (moderato). Requires L1_RPC_URL and PRIVATE_KEY env vars. Run max-approve-portal first.')]
send-deposit to amount="1000000" memo="0x0000000000000000000000000000000000000000000000000000000000000000":
    #!/bin/bash
    set -euo pipefail
    RPC="${L1_RPC_URL:?Set L1_RPC_URL env var}"
    PK="${PRIVATE_KEY:?Set PRIVATE_KEY env var}"
    PORTAL="0x1bc99e6a8c4689f1884527152ba542f012316149"
    HTTP_RPC=$(echo "$RPC" | sed 's|^wss://|https://|' | sed 's|^ws://|http://|')
    echo "Depositing {{amount}} to {{to}}..."
    cast send "$PORTAL" "deposit(address,uint128,bytes32)" "{{to}}" "{{amount}}" "{{memo}}" \
        --rpc-url "$HTTP_RPC" --private-key "$PK"
    echo "Deposit sent!"

[group('zone')]
[doc('Creates a new zone on L1 via ZoneFactory, generates zone genesis, and launches the zone node. Requires L1_RPC_URL, PRIVATE_KEY, ZONE_TOKEN, and SEQUENCER_KEY env vars.')]
zone-launch reset="true" args="":
    #!/bin/bash
    set -euo pipefail
    PK="${PRIVATE_KEY:?Set PRIVATE_KEY env var}"
    ZONE_TOKEN_ADDR="${ZONE_TOKEN:?Set ZONE_TOKEN env var}"
    SEQ_KEY="${SEQUENCER_KEY:?Set SEQUENCER_KEY env var}"
    L1_RPC="${L1_RPC_URL:?Set L1_RPC_URL env var (wss://...)}"
    HTTP_RPC=$(echo "$L1_RPC" | sed 's|^wss://|https://|' | sed 's|^ws://|http://|')

    # Derive sequencer address from the sequencer key
    SEQUENCER_ADDR=$(cast wallet address "$SEQ_KEY")

    ZONE_DIR="/tmp/tempo-zone"

    if [[ "{{reset}}" = "true" ]]; then
        rm -rf "$ZONE_DIR" || true
        mkdir -p "$ZONE_DIR"
    fi

    # Build solidity specs first (needed for genesis artifact loading)
    # Run in subshell so CWD is preserved even if forge returns non-zero
    echo "Building Solidity specs..."
    (cd docs/specs && forge build --skip test) || true

    # Step 1: Create zone on L1 + generate genesis
    echo "Creating zone on L1 and generating genesis..."
    CREATE_OUTPUT=$(cargo run -p tempo-xtask -- create-zone \
        --output "$ZONE_DIR" \
        --l1-rpc-url "$HTTP_RPC" \
        --zone-token "$ZONE_TOKEN_ADDR" \
        --sequencer "$SEQUENCER_ADDR" \
        --private-key "$PK" 2>&1 | tee /dev/stderr)

    # Extract portal address from create-zone output (line: "  Portal: 0x...")
    PORTAL_ADDR=$(echo "$CREATE_OUTPUT" | sed -n 's/.*Portal: \(0x[0-9a-fA-F]*\).*/\1/p')
    if [[ -z "$PORTAL_ADDR" ]]; then
        echo "ERROR: Failed to extract portal address from create-zone output"
        exit 1
    fi

    # Extract Tempo anchor block number (line: "  Tempo anchor block: <number>")
    GENESIS_L1_BLOCK=$(echo "$CREATE_OUTPUT" | sed -n 's/.*Tempo anchor block: \([0-9]*\).*/\1/p')
    if [[ -z "$GENESIS_L1_BLOCK" ]]; then
        echo "ERROR: Failed to extract genesis L1 block number from create-zone output"
        exit 1
    fi

    echo "Zone genesis generated at $ZONE_DIR/genesis.json"
    echo "Portal address: $PORTAL_ADDR"
    echo "Genesis L1 block: $GENESIS_L1_BLOCK"

    # Step 2: Launch zone node
    echo "Launching Tempo Zone node..."
    cargo run --bin tempo-zone -- \
        node \
        --chain "$ZONE_DIR/genesis.json" \
        --dev \
        --dev.block-time 1sec \
        --l1.rpc-url "$L1_RPC" \
        --l1.portal-address "${L1_PORTAL_ADDRESS:-$PORTAL_ADDR}" \
        --l1.token-address "$ZONE_TOKEN_ADDR" \
        --l1.genesis-block-number "$GENESIS_L1_BLOCK" \
        --http \
        --http.addr 0.0.0.0 \
        --http.port 8546 \
        --http.api all \
        --datadir "$ZONE_DIR" \
        --log.file.directory "$ZONE_DIR/logs" \
        --sequencer.key "$SEQ_KEY" \
        {{args}}

[group('zone')]
[doc('Starts a Tempo Zone L2 node in dev mode, subscribing to L1 deposits')]
zoneup reset="true" args="":
    #!/bin/bash
    if [[ "{{reset}}" = "true" ]]; then
        rm -rf /tmp/tempo-zone || true
    fi;
    cargo run --bin tempo-zone -- \
                      node \
                      --dev \
                      --dev.block-time 1sec \
                      --l1.rpc-url "${L1_RPC_URL:?Set L1_RPC_URL env var (wss://...)}" \
                      --l1.portal-address 0x1bc99e6a8c4689f1884527152ba542f012316149 \
                      --l1.token-address 0x20C0000000000000000000000000000000000000 \
                      --http \
                      --http.addr 0.0.0.0 \
                      --http.port 8546 \
                      --http.api all \
                      --datadir /tmp/tempo-zone \
                      --log.file.directory /tmp/tempo-zone/logs \
                      ${SEQUENCER_KEY:+--sequencer.key $SEQUENCER_KEY} \
                      {{args}}

[group('zone')]
[doc('Approves the ZoneOutbox to spend max zone tokens on L2. Requires PRIVATE_KEY env var.')]
max-approve-outbox token="0x20C0000000000000000000000000000000000000" rpc="http://localhost:8546":
    #!/bin/bash
    set -euo pipefail
    PK="${PRIVATE_KEY:?Set PRIVATE_KEY env var}"
    OUTBOX="0x1c00000000000000000000000000000000000002"
    echo "Approving ZoneOutbox for max zone tokens..."
    cast send "{{token}}" "approve(address,uint256)" "$OUTBOX" "$(cast max-uint)" \
        --rpc-url "{{rpc}}" --private-key "$PK"
    echo "Approved!"

[group('zone')]
[doc('Sends a withdrawal request on the zone (L2) back to Tempo L1. Requires PRIVATE_KEY env var. Run max-approve-outbox first.')]
send-withdrawal to amount="1000000" memo="0x0000000000000000000000000000000000000000000000000000000000000000" gas-limit="0" fallback-recipient="" data="0x" rpc="http://localhost:8546":
    #!/bin/bash
    set -euo pipefail
    PK="${PRIVATE_KEY:?Set PRIVATE_KEY env var}"
    OUTBOX="0x1c00000000000000000000000000000000000002"
    # Default fallback-recipient to sender if not provided
    FALLBACK="{{fallback-recipient}}"
    if [[ -z "$FALLBACK" ]]; then
        FALLBACK=$(cast wallet address "$PK")
    fi
    echo "Requesting withdrawal of {{amount}} to {{to}} (fallback: $FALLBACK)..."
    cast send "$OUTBOX" \
        "requestWithdrawal(address,uint128,bytes32,uint64,address,bytes)" \
        "{{to}}" "{{amount}}" "{{memo}}" "{{gas-limit}}" "$FALLBACK" "{{data}}" \
        --rpc-url "{{rpc}}" --private-key "$PK"
    echo "Withdrawal requested!"

[group('zone')]
[doc('Checks TIP-20 token balance for an account on the zone (port 8546)')]
check-balance account token="0x20C0000000000000000000000000000000000000" rpc="http://localhost:8546":
    @printf "Balance of {{account}}: " && cast call "{{token}}" "balanceOf(address)(uint256)" "{{account}}" --rpc-url "{{rpc}}"

mod scripts

[group('dev')]
tempo-dev-up: scripts::tempo-dev-up
tempo-dev-down: scripts::tempo-dev-down

[group('test')]
feature-test: scripts::auto-7702-delegation  scripts::basic-transfer scripts::registrar-delegation scripts::create-tip20-token scripts::fee-amm

fee-amm: scripts::fee-amm

# Docs commands
[group('docs')]
[doc('Install docs dependencies')]
docs-install:
    cd docs && bun install

[group('docs')]
[doc('Start docs dev server')]
docs-dev:
    cd docs && bun run dev

[group('docs')]
[doc('Build docs for production')]
docs-build:
    cd docs && bun run build

[group('docs')]
[doc('Run docs linting and type checks')]
docs-check:
    cd docs && bun run check && bun run check:types

[group('docs')]
[doc('Run Solidity specs tests')]
docs-specs-test:
    cd docs/specs && forge test -vvv

[group('docs')]
[doc('Build Solidity specs')]
docs-specs-build:
    cd docs/specs && forge build --sizes
