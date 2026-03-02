cross_compile := "false"
cargo_build_binary := if cross_compile == "true" { "cross" } else { "cargo" }
act_debug_mode := env("ACT", "false")
zone_rpc := env("ZONE_RPC_URL", "http://localhost:8546")
zone_http_port := env("ZONE_HTTP_PORT", "8546")

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
send-deposit amount="1000000" to="" token="0x20C0000000000000000000000000000000000000" memo="0x0000000000000000000000000000000000000000000000000000000000000000":
    #!/bin/bash
    set -euo pipefail
    RPC="${L1_RPC_URL:?Set L1_RPC_URL env var}"
    PK="${PRIVATE_KEY:?Set PRIVATE_KEY env var}"
    PORTAL="${L1_PORTAL_ADDRESS:?Set L1_PORTAL_ADDRESS env var}"
    TO="{{to}}"
    if [[ -z "$TO" ]]; then
        TO=$(cast wallet address "$PK")
    fi
    echo "Depositing {{amount}} to $TO..."
    TX_OUTPUT=$(cast send "$PORTAL" "deposit(address,address,uint128,bytes32)" "{{token}}" "$TO" "{{amount}}" "{{memo}}" \
        --rpc-url "$RPC" --private-key "$PK" --json)
    TX_HASH=$(echo "$TX_OUTPUT" | jq -r '.transactionHash')
    L1_BLOCK=$(echo "$TX_OUTPUT" | jq -r '.blockNumber')
    L1_BLOCK_DEC=$(printf '%d' "$L1_BLOCK")
    echo "Deposit sent! (block $L1_BLOCK_DEC)"
    echo "Explorer: https://explore.moderato.tempo.xyz/tx/$TX_HASH"

[group('zone')]
[doc('Sends an encrypted deposit to the ZonePortal on L1 (recipient and memo are hidden on-chain). Requires L1_RPC_URL, L1_PORTAL_ADDRESS, and PRIVATE_KEY env vars. Run max-approve-portal first.')]
send-deposit-encrypted amount="1000000" to="" memo="0x0000000000000000000000000000000000000000000000000000000000000000" token="0x20C0000000000000000000000000000000000000" rpc=zone_rpc:
    #!/bin/bash
    set -euo pipefail
    PK="${PRIVATE_KEY:?Set PRIVATE_KEY env var}"
    TO="{{to}}"
    if [[ -z "$TO" ]]; then
        TO=$(cast wallet address "$PK")
    fi
    ARGS="--amount {{amount}} --token {{token}} --memo {{memo}} --to $TO --zone-rpc-url {{rpc}}"
    cargo run -p tempo-xtask -- encrypted-deposit --private-key "$PK" $ARGS

[group('zone')]
[doc('Fetches and prints zone info from the ZoneFactory. Pass a zone ID (integer) or portal address (0x...).')]
zone-info identifier:
    cargo run -p tempo-xtask -- zone-info {{identifier}}

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
[doc('Starts a Tempo Zone L2 node, subscribing to L1 deposits. Pass the zone name used in create-zone. Use profile=release for production.')]
zone-up name reset="false" profile="dev" args="":
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
    ZONE_ID=$(jq -r '.zoneId' "$ZONE_JSON")
    SEQ_KEY="${SEQUENCER_KEY:-$(jq -r '.sequencerKey // empty' "$ZONE_JSON")}"
    if [[ -z "$SEQ_KEY" ]]; then
        echo "Error: SEQUENCER_KEY env var not set and not found in $ZONE_JSON" >&2
        exit 1
    fi
    DATADIR="/tmp/tempo-zone-{{name}}"
    if [[ "{{reset}}" = "true" ]]; then
        rm -rf "$DATADIR" || true
    fi
    PROFILE_FLAG=""
    if [[ "{{profile}}" == "release" ]]; then
        PROFILE_FLAG="--release"
    elif [[ "{{profile}}" != "dev" ]]; then
        PROFILE_FLAG="--profile {{profile}}"
    fi
    cargo run $PROFILE_FLAG --bin tempo-zone -- \
                      node \
                      --chain "$GENESIS_JSON" \
                      --l1.rpc-url "${L1_RPC_URL:?Set L1_RPC_URL env var (wss://...)}" \
                      --l1.portal-address "$PORTAL" \
                      --l1.genesis-block-number "$ANCHOR_BLOCK" \
                      --zone.id "$ZONE_ID" \
                      --http \
                      --http.addr 0.0.0.0 \
                      --http.port {{zone_http_port}} \
                      --http.api all \
                      --datadir "$DATADIR" \
                      --log.file.directory "$DATADIR/logs" \
                      --sequencer-key "$SEQ_KEY" \
                      {{args}}

[group('zone')]
[doc('Approves the ZoneOutbox to spend max zone tokens on L2. Requires PRIVATE_KEY env var.')]
max-approve-outbox token="0x20C0000000000000000000000000000000000000" rpc=zone_rpc:
    #!/bin/bash
    set -euo pipefail
    PK="${PRIVATE_KEY:?Set PRIVATE_KEY env var}"
    OUTBOX="0x1c00000000000000000000000000000000000002"
    echo "Approving ZoneOutbox for max zone tokens..."
    cast send "{{token}}" "approve(address,uint256)" "$OUTBOX" "$(cast max-uint)" \
        --rpc-url "{{rpc}}" --private-key "$PK" --gas-limit 100000
    echo "Approved!"

[group('zone')]
[doc('Sends a withdrawal request on the zone (L2) back to Tempo L1. Requires PRIVATE_KEY env var. Run max-approve-outbox first.')]
send-withdrawal amount="1000000" to="" token="0x20C0000000000000000000000000000000000000" memo="0x0000000000000000000000000000000000000000000000000000000000000000" gas-limit="0" fallback-recipient="" data="0x" rpc=zone_rpc:
    #!/bin/bash
    set -euo pipefail
    PK="${PRIVATE_KEY:?Set PRIVATE_KEY env var}"
    OUTBOX="0x1c00000000000000000000000000000000000002"
    TO="{{to}}"
    if [[ -z "$TO" ]]; then
        TO=$(cast wallet address "$PK")
    fi
    FALLBACK="{{fallback-recipient}}"
    if [[ -z "$FALLBACK" ]]; then
        FALLBACK="$TO"
    fi
    echo "Requesting withdrawal of {{amount}} to $TO (fallback: $FALLBACK)..."
    L2_OUTPUT=$(cast send "$OUTBOX" \
        "requestWithdrawal(address,address,uint128,bytes32,uint64,address,bytes)" \
        "{{token}}" "$TO" "{{amount}}" "{{memo}}" "{{gas-limit}}" "$FALLBACK" "{{data}}" \
        --rpc-url "{{rpc}}" --private-key "$PK" --gas-limit 500000 --json)
    L2_TX=$(echo "$L2_OUTPUT" | jq -r '.transactionHash')
    L2_BLOCK=$(echo "$L2_OUTPUT" | jq -r '.blockNumber')
    echo "Withdrawal requested on L2! tx: $L2_TX (block $(printf '%d' "$L2_BLOCK"))"

    # Wait for the withdrawal to be processed on L1
    L1_RPC="${L1_RPC_URL:-}"
    PORTAL="${L1_PORTAL_ADDRESS:-}"
    if [[ -z "$L1_RPC" || -z "$PORTAL" ]]; then
        echo "Set L1_RPC_URL and L1_PORTAL_ADDRESS env vars to wait for L1 processing."
        exit 0
    fi
    HTTP_RPC=$(echo "$L1_RPC" | sed 's|^wss://|https://|' | sed 's|^ws://|http://|')
    FROM_BLOCK=$(cast block-number --rpc-url "$HTTP_RPC")
    echo "Waiting for withdrawal to be processed on L1 (from block $FROM_BLOCK)..."
    while true; do
        LOGS=$(cast logs --address "$PORTAL" --from-block "$FROM_BLOCK" --rpc-url "$HTTP_RPC" \
            "WithdrawalProcessed(address indexed to, address token, uint128 amount, bool callbackSuccess)" \
            "$TO" --json 2>/dev/null || echo "[]")
        if [[ "$LOGS" != "[]" && "$LOGS" != "" && "$LOGS" != "null" ]]; then
            L1_TX=$(echo "$LOGS" | jq -r '.[-1].transactionHash')
            L1_BLOCK=$(echo "$LOGS" | jq -r '.[-1].blockNumber')
            L1_BLOCK_DEC=$(printf '%d' "$L1_BLOCK")
            echo "Withdrawal processed on L1! (block $L1_BLOCK_DEC)"
            echo "Explorer: https://explore.moderato.tempo.xyz/tx/$L1_TX"
            break
        fi
        sleep 0.25
    done

[group('zone')]
[doc('Enables a TIP-20 token on the ZonePortal for bridging. Token can be an address or alias (pathusd, alphausd, betausd). Requires L1_RPC_URL, L1_PORTAL_ADDRESS, and SEQUENCER_KEY env vars.')]
enable-token token:
    #!/bin/bash
    set -euo pipefail
    RPC="${L1_RPC_URL:?Set L1_RPC_URL env var}"
    PK="${SEQUENCER_KEY:?Set SEQUENCER_KEY env var (only the sequencer can enable tokens)}"
    PORTAL="${L1_PORTAL_ADDRESS:?Set L1_PORTAL_ADDRESS env var}"
    HTTP_RPC=$(echo "$RPC" | sed 's|^wss://|https://|' | sed 's|^ws://|http://|')
    TOKEN="{{token}}"
    # Resolve well-known aliases (lowercased for case-insensitive matching)
    TOKEN_LOWER=$(echo "$TOKEN" | tr '[:upper:]' '[:lower:]')
    case "$TOKEN_LOWER" in
        pathusd|path-usd|path_usd)
            TOKEN="0x20C0000000000000000000000000000000000000" ;;
        alphausd|alpha-usd|alpha_usd)
            TOKEN="0x20c0000000000000000000000000000000000001" ;;
        betausd|beta-usd|beta_usd)
            TOKEN="0x20c0000000000000000000000000000000000002" ;;
    esac
    echo "Enabling token $TOKEN on portal $PORTAL..."
    TX_OUTPUT=$(cast send "$PORTAL" "enableToken(address)" "$TOKEN" \
        --rpc-url "$HTTP_RPC" --private-key "$PK" --json)
    TX_HASH=$(echo "$TX_OUTPUT" | jq -r '.transactionHash')
    L1_BLOCK=$(echo "$TX_OUTPUT" | jq -r '.blockNumber')
    L1_BLOCK_DEC=$(printf '%d' "$L1_BLOCK")
    echo "L1 tx: $TX_HASH (block $L1_BLOCK_DEC)"
    echo "Explorer: https://explore.moderato.tempo.xyz/tx/$TX_HASH"
    # Read token metadata from L1
    NAME=$(cast call "$TOKEN" "name()(string)" --rpc-url "$HTTP_RPC" 2>/dev/null || echo "???")
    SYMBOL=$(cast call "$TOKEN" "symbol()(string)" --rpc-url "$HTTP_RPC" 2>/dev/null || echo "???")
    echo "Waiting for zone to process L1 block $L1_BLOCK_DEC (token: $NAME / $SYMBOL)..."
    ZONE_RPC="${ZONE_RPC_URL:-http://localhost:8546}"
    INBOX="0x1c00000000000000000000000000000000000001"
    while true; do
        LOGS=$(cast logs --address "$INBOX" --from-block 1 --rpc-url "$ZONE_RPC" \
            "TokenEnabled(address indexed token, string name, string symbol, string currency)" \
            "$TOKEN" --json 2>/dev/null || echo "[]")
        if [[ "$LOGS" != "[]" && "$LOGS" != "" && "$LOGS" != "null" ]]; then
            echo "✅ Token enabled on zone: $NAME ($SYMBOL) at $TOKEN"
            break
        fi
        sleep 0.5
    done

[group('zone')]
[doc('Checks TIP-20 token balance for an account on the zone (port 8546)')]
check-balance account token="0x20C0000000000000000000000000000000000000" rpc=zone_rpc:
    @printf "Balance of {{account}}: " && cast call "{{token}}" "balanceOf(address)(uint256)" "{{account}}" --rpc-url "{{rpc}}"

[group('zone')]
[doc('Generates a signed auth token for the private zone RPC. Requires PRIVATE_KEY env var. Reads zone metadata from generated/<name>/zone.json.')]
zone-auth-token name:
    #!/bin/bash
    set -euo pipefail
    PK="${PRIVATE_KEY:?Set PRIVATE_KEY env var}"
    ZONE_JSON="generated/{{name}}/zone.json"
    if [[ ! -f "$ZONE_JSON" ]]; then
        echo "Error: $ZONE_JSON not found. Run 'just create-zone {{name}}' first." >&2
        exit 1
    fi
    ZONE_ID=$(jq -r '.zoneId' "$ZONE_JSON")
    PORTAL=$(jq -r '.portal' "$ZONE_JSON")
    GENESIS_JSON="generated/{{name}}/genesis.json"
    CHAIN_ID=$(jq -r '.config.chainId' "$GENESIS_JSON")
    NOW=$(date +%s)
    EXPIRES=$((NOW + 600))
    MAGIC="54656d706f5a6f6e655250430000000000000000000000000000000000000000"
    VERSION="00"
    ZONE_ID_HEX=$(printf '%016x' "$ZONE_ID")
    CHAIN_ID_HEX=$(printf '%016x' "$CHAIN_ID")
    PORTAL_HEX=$(echo "$PORTAL" | sed 's/0x//' | tr '[:upper:]' '[:lower:]')
    ISSUED_HEX=$(printf '%016x' "$NOW")
    EXPIRES_HEX=$(printf '%016x' "$EXPIRES")
    FIELDS="${VERSION}${ZONE_ID_HEX}${CHAIN_ID_HEX}${PORTAL_HEX}${ISSUED_HEX}${EXPIRES_HEX}"
    DIGEST=$(cast keccak "0x${MAGIC}${FIELDS}")
    SIG=$(cast wallet sign --no-hash "$DIGEST" --private-key "$PK")
    SIG_HEX=$(echo "$SIG" | sed 's/0x//')
    echo "${SIG_HEX}${FIELDS}"

[group('zone')]
[doc('Checks TIP-20 token balance via the private RPC (with auth token). Requires PRIVATE_KEY env var.')]
check-balance-private name token="0x20C0000000000000000000000000000000000000" rpc="http://localhost:8544":
    #!/bin/bash
    set -euo pipefail
    PK="${PRIVATE_KEY:?Set PRIVATE_KEY env var}"
    ACCOUNT=$(cast wallet address "$PK")
    TOKEN=$(just zone-auth-token {{name}})
    ACCOUNT_LOWER=$(echo "$ACCOUNT" | sed 's/0x//' | tr '[:upper:]' '[:lower:]')
    RESULT=$(curl -s -X POST "{{rpc}}" \
        -H "Content-Type: application/json" \
        -H "x-authorization-token: ${TOKEN}" \
        -d "{\"jsonrpc\":\"2.0\",\"method\":\"eth_call\",\"params\":[{\"from\":\"$ACCOUNT\",\"to\":\"{{token}}\",\"data\":\"0x70a08231000000000000000000000000${ACCOUNT_LOWER}\"}],\"id\":1}")
    RAW=$(echo "$RESULT" | jq -r '.result // empty')
    ERROR=$(echo "$RESULT" | jq -r '.error.message // empty')
    if [[ -n "$ERROR" ]]; then
        echo "RPC error: $ERROR"
        exit 1
    fi
    if [[ -z "$RAW" ]]; then
        echo "No result from RPC"
        echo "$RESULT"
        exit 1
    fi
    BALANCE=$(cast --to-dec "$RAW")
    echo "Balance of $ACCOUNT: $BALANCE"

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

    # Save sequencer key into zone.json for later use
    jq --arg sk "$SEQUENCER_KEY" --arg sa "$SEQUENCER_ADDR" \
        '. + {sequencerKey: $sk, sequencerAddress: $sa}' "$OUTPUT/zone.json" > "$OUTPUT/zone.json.tmp" \
        && mv "$OUTPUT/zone.json.tmp" "$OUTPUT/zone.json"

    PORTAL=$(jq -r '.portal' "$OUTPUT/zone.json")
    ZONE_ID=$(jq -r '.zoneId' "$OUTPUT/zone.json")
    ANCHOR_BLOCK=$(jq -r '.tempoAnchorBlock' "$OUTPUT/zone.json")

    # Step 5: Register sequencer encryption key on the portal
    echo "Step 5: Registering sequencer encryption key on ZonePortal..."
    cargo run -p tempo-xtask -- set-encryption-key \
        --l1-rpc-url "$HTTP_RPC" \
        --portal "$PORTAL" \
        --private-key "$SEQUENCER_KEY"
    echo ""

    # Step 6: Display summary
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
    echo "  Sequencer key saved to $OUTPUT/zone.json"
    echo ""

    # Step 7: Build and start the zone node
    echo "Step 7: Building and starting zone node (release)..."
    echo ""
    cargo build --bin tempo-zone --release
    DATADIR="/tmp/tempo-zone-{{name}}"
    rm -rf "$DATADIR" || true
    exec cargo run --release --bin tempo-zone -- \
                      node \
                      --chain "$OUTPUT/genesis.json" \
                      --l1.rpc-url "$L1_RPC" \
                      --l1.portal-address "$PORTAL" \
                      --l1.genesis-block-number "$ANCHOR_BLOCK" \
                      --zone.id "$ZONE_ID" \
                      --http \
                      --http.addr 0.0.0.0 \
                      --http.port {{zone_http_port}} \
                      --http.api all \
                      --datadir "$DATADIR" \
                      --log.file.directory "$DATADIR/logs" \
                      --sequencer-key "$SEQUENCER_KEY"

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
