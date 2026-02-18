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
localnet accounts="1000" reset="false" profile="maxperf" features="asm-keccak" args="":
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
    PORTAL="${L1_PORTAL_ADDRESS:?Set L1_PORTAL_ADDRESS env var}"
    TOKEN="0x20C0000000000000000000000000000000000000"
    HTTP_RPC=$(echo "$RPC" | sed 's|^wss://|https://|' | sed 's|^ws://|http://|')
    echo "Approving ZonePortal for max TEMPO..."
    cast send "$TOKEN" "approve(address,uint256)" "$PORTAL" "$(cast max-uint)" \
        --rpc-url "$HTTP_RPC" --private-key "$PK"
    echo "Approved!"

[group('zone')]
[doc('Sends a test deposit to the ZonePortal on L1 (moderato). Requires L1_RPC_URL and PRIVATE_KEY env vars. Run max-approve-portal first.')]
send-deposit to amount="1000000" token="0x20C0000000000000000000000000000000000000" memo="0x0000000000000000000000000000000000000000000000000000000000000000":
    #!/bin/bash
    set -euo pipefail
    RPC="${L1_RPC_URL:?Set L1_RPC_URL env var}"
    PK="${PRIVATE_KEY:?Set PRIVATE_KEY env var}"
    PORTAL="${L1_PORTAL_ADDRESS:?Set L1_PORTAL_ADDRESS env var}"
    HTTP_RPC=$(echo "$RPC" | sed 's|^wss://|https://|' | sed 's|^ws://|http://|')
    echo "Depositing {{amount}} to {{to}}..."
    cast send "$PORTAL" "deposit(address,address,uint128,bytes32)" "{{token}}" "{{to}}" "{{amount}}" "{{memo}}" \
        --rpc-url "$HTTP_RPC" --private-key "$PK"
    echo "Deposit sent!"

[group('zone')]
[doc('Creates a new zone on L1 via ZoneFactory and generates genesis + zone.json in generated/<name>/. Requires L1_RPC_URL, PRIVATE_KEY, and SEQUENCER_KEY env vars.')]
create-zone name:
    #!/bin/bash
    set -euo pipefail
    PK="${PRIVATE_KEY:?Set PRIVATE_KEY env var}"
    ZONE_TOKEN_L1="${ZONE_TOKEN:-0x20C0000000000000000000000000000000000000}"
    SEQ_KEY="${SEQUENCER_KEY:?Set SEQUENCER_KEY env var}"
    L1_RPC="${L1_RPC_URL:?Set L1_RPC_URL env var (wss://...)}"
    HTTP_RPC=$(echo "$L1_RPC" | sed 's|^wss://|https://|' | sed 's|^ws://|http://|')
    SEQUENCER_ADDR=$(cast wallet address "$SEQ_KEY")
    OUTPUT="generated/{{name}}"
    mkdir -p "$OUTPUT"
    echo "Building Solidity specs..."
    (cd docs/specs && forge build --skip test) || true
    echo "Building xtask..."
    cargo build -p tempo-xtask
    echo "Creating zone '{{name}}' on L1 and generating genesis..."
    cargo run -p tempo-xtask -- create-zone \
        --output "$OUTPUT" \
        --l1-rpc-url "$HTTP_RPC" \
        --initial-token "$ZONE_TOKEN_L1" \
        --sequencer "$SEQUENCER_ADDR" \
        --private-key "$PK"
    echo "Zone '{{name}}' created. Artifacts in $OUTPUT/"

[group('zone')]
[doc('Starts a Tempo Zone L2 node, subscribing to L1 deposits. Pass the zone name used in create-zone.')]
zone-up name reset="false" args="":
    #!/bin/bash
    set -euo pipefail
    ZONE_DIR="generated/{{name}}"
    ZONE_JSON="$ZONE_DIR/zone.json"
    GENESIS_JSON="$ZONE_DIR/genesis.json"
    if [[ ! -f "$ZONE_JSON" ]]; then
        echo "Error: $ZONE_JSON not found. Run 'just create-zone {{name}}' first." >&2
        exit 1
    fi
    if [[ ! -f "$GENESIS_JSON" ]]; then
        echo "Error: $GENESIS_JSON not found. Run 'just create-zone {{name}}' first." >&2
        exit 1
    fi
    PORTAL=$(jq -r '.portal' "$ZONE_JSON")
    ANCHOR_BLOCK=$(jq -r '.tempoAnchorBlock' "$ZONE_JSON")
    DATADIR="/tmp/tempo-zone-{{name}}"
    if [[ "{{reset}}" = "true" ]]; then
        rm -rf "$DATADIR" || true
    fi
    cargo run --bin tempo-zone -- \
                      node \
                      --chain "$GENESIS_JSON" \
                      --l1.rpc-url "${L1_RPC_URL:?Set L1_RPC_URL env var (wss://...)}" \
                      --l1.portal-address "$PORTAL" \
                      --l1.genesis-block-number "$ANCHOR_BLOCK" \
                      --http \
                      --http.addr 0.0.0.0 \
                      --http.port 8546 \
                      --http.api all \
                      --datadir "$DATADIR" \
                      --log.file.directory "$DATADIR/logs" \
                      ${SEQUENCER_KEY:+--sequencer-key $SEQUENCER_KEY} \
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
send-withdrawal to amount="1000000" token="0x20C0000000000000000000000000000000000000" memo="0x0000000000000000000000000000000000000000000000000000000000000000" gas-limit="0" fallback-recipient="" data="0x" rpc="http://localhost:8546":
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
        "requestWithdrawal(address,address,uint128,bytes32,uint64,address,bytes)" \
        "{{token}}" "{{to}}" "{{amount}}" "{{memo}}" "{{gas-limit}}" "$FALLBACK" "{{data}}" \
        --rpc-url "{{rpc}}" --private-key "$PK"
    echo "Withdrawal requested!"

[group('zone')]
[doc('Checks TIP-20 token balance for an account on the zone (port 8546)')]
check-balance account token="0x20C0000000000000000000000000000000000000" rpc="http://localhost:8546":
    @printf "Balance of {{account}}: " && cast call "{{token}}" "balanceOf(address)(uint256)" "{{account}}" --rpc-url "{{rpc}}"

[group('zone')]
[doc('End-to-end: generates a sequencer key, funds it on L1, creates a zone on-chain, generates genesis, and starts the zone node. Requires L1_RPC_URL env var.')]
deploy-zone name:
    #!/bin/bash
    set -euo pipefail
    L1_RPC="${L1_RPC_URL:?Set L1_RPC_URL env var (wss://...)}"
    HTTP_RPC=$(echo "$L1_RPC" | sed 's|^wss://|https://|' | sed 's|^ws://|http://|')
    OUTPUT="generated/{{name}}"

    echo "============================================"
    echo "  Deploying Zone: {{name}}"
    echo "============================================"
    echo ""

    # Step 1: Generate a new sequencer keypair
    echo "Step 1: Generating sequencer keypair..."
    KEY_OUTPUT=$(cast wallet new 2>/dev/null)
    SEQUENCER_ADDR=$(echo "$KEY_OUTPUT" | grep 'Address:' | awk '{print $2}')
    SEQUENCER_KEY=$(echo "$KEY_OUTPUT" | grep 'Private key:' | awk '{print $3}')
    echo "  Sequencer address: $SEQUENCER_ADDR"
    echo ""

    # Step 2: Fund the sequencer on L1
    echo "Step 2: Funding sequencer on L1 (via tempo_fundAddress)..."
    cast rpc tempo_fundAddress "$SEQUENCER_ADDR" --rpc-url "$HTTP_RPC" > /dev/null 2>&1
    echo "  Funded! Check: https://explore.moderato.tempo.xyz/address/$SEQUENCER_ADDR"
    echo ""

    # Step 3: Build Solidity specs
    echo "Step 3: Building Solidity specs..."
    (cd docs/specs && forge build --skip test) || true
    echo ""

    # Step 4: Create zone on L1 and generate genesis
    echo "Step 4: Creating zone on L1 via ZoneFactory..."
    mkdir -p "$OUTPUT"
    cargo run -p tempo-xtask -- create-zone \
        --output "$OUTPUT" \
        --l1-rpc-url "$HTTP_RPC" \
        --sequencer "$SEQUENCER_ADDR" \
        --private-key "$SEQUENCER_KEY"
    echo ""

    # Step 5: Display summary
    PORTAL=$(jq -r '.portal' "$OUTPUT/zone.json")
    ZONE_ID=$(jq -r '.zoneId' "$OUTPUT/zone.json")
    ANCHOR_BLOCK=$(jq -r '.tempoAnchorBlock' "$OUTPUT/zone.json")

    echo "============================================"
    echo "  Zone Deployed Successfully!"
    echo "============================================"
    echo ""
    echo "  Zone ID:         $ZONE_ID"
    echo "  Zone Name:       {{name}}"
    echo "  Portal:          $PORTAL"
    echo "  Sequencer:       $SEQUENCER_ADDR"
    echo "  Anchor Block:    $ANCHOR_BLOCK"
    echo ""
    echo "  Artifacts:       $OUTPUT/"
    echo "  Genesis:         $OUTPUT/genesis.json"
    echo "  Zone Metadata:   $OUTPUT/zone.json"
    echo ""
    echo "  Explorer:        https://explore.moderato.tempo.xyz/address/$PORTAL"
    echo ""
    echo "  ⚠️  SAVE YOUR SEQUENCER KEY:"
    echo "  export SEQUENCER_KEY=$SEQUENCER_KEY"
    echo ""
    echo "  To start the zone node:"
    echo "  export L1_RPC_URL=\"$L1_RPC\""
    echo "  export SEQUENCER_KEY=\"$SEQUENCER_KEY\""
    echo "  just zone-up {{name}}"

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
