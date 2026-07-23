#!/usr/bin/env bash
set -euo pipefail

readonly FACTORY_ADDRESS="0x5aF2000000000000000000000000000000000000"
readonly FOUNDRY_ROOT="specs/ref-impls"

readonly -a CONTRACTS=("ZonePortal" "ZoneMessenger" "Verifier")
readonly -a TARGETS=(
    "0x5AD1000000000000000000000000000000000000"
    "0x5A4d000000000000000000000000000000000000"
    "0x5a56000000000000000000000000000000000000"
)
readonly -a SETTERS=(
    "setPortalImplementation"
    "setZoneMessengerImplementation"
    "setVerifierImplementation"
)

: "${RPC_URL:?RPC_URL must be set}"

declare -a desired_runtimes=()
declare -a desired_hashes=()
declare -a changed_indices=()

for index in "${!CONTRACTS[@]}"; do
    contract="${CONTRACTS[$index]}"
    target="${TARGETS[$index]}"
    runtime=$(forge inspect \
        --root "$FOUNDRY_ROOT" \
        "src/tempo/${contract}.sol:${contract}" \
        deployedBytecode)

    if [[ "$runtime" == "0x" || "$runtime" == "0X" || -z "$runtime" ]]; then
        echo "Built $contract runtime is empty" >&2
        exit 1
    fi

    desired_hash=$(cast keccak "$runtime" | tr '[:upper:]' '[:lower:]')
    installed_runtime=$(cast code "$target" --rpc-url "$RPC_URL")
    installed_hash=$(cast keccak "$installed_runtime" | tr '[:upper:]' '[:lower:]')

    desired_runtimes[$index]="$runtime"
    desired_hashes[$index]="$desired_hash"

    if [[ "$installed_hash" == "$desired_hash" ]]; then
        echo "$contract is unchanged ($desired_hash)"
    else
        echo "$contract changed: $installed_hash -> $desired_hash"
        changed_indices+=("$index")
    fi
done

if (( ${#changed_indices[@]} == 0 )); then
    echo "All nextfork Zone runtimes are current."
    if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
        echo "All nextfork Zone runtimes already match this revision." >> "$GITHUB_STEP_SUMMARY"
    fi
    exit 0
fi

: "${FACTORY_OWNER_PRIVATE_KEY:?ZONE_FACTORY_OWNER_PRIVATE_KEY secret must be set}"

signer=$(cast wallet address "$FACTORY_OWNER_PRIVATE_KEY" | tr '[:upper:]' '[:lower:]')
owner=$(cast call "$FACTORY_ADDRESS" "owner()(address)" --rpc-url "$RPC_URL" | \
    tr '[:upper:]' '[:lower:]')
if [[ "$signer" != "$owner" ]]; then
    echo "Configured signer $signer is not the ZoneFactory owner $owner" >&2
    exit 1
fi

updates_locked=$(cast call \
    "$FACTORY_ADDRESS" \
    "implementationUpdatesLocked()(bool)" \
    --rpc-url "$RPC_URL")
if [[ "$updates_locked" != "false" ]]; then
    echo "ZoneFactory implementation updates are locked" >&2
    exit 1
fi

for index in "${changed_indices[@]}"; do
    contract="${CONTRACTS[$index]}"
    target="${TARGETS[$index]}"
    setter="${SETTERS[$index]}"
    runtime="${desired_runtimes[$index]#0x}"
    runtime_size=$(( ${#runtime} / 2 ))

    if (( runtime_size > 65535 )); then
        echo "$contract runtime is too large to wrap in PUSH2 initcode" >&2
        exit 1
    fi

    printf -v runtime_size_hex '%04x' "$runtime_size"
    initcode="0x61${runtime_size_hex}61000f60003961${runtime_size_hex}6000f3${runtime}"

    echo "Deploying $contract runtime source..."
    deployment_receipt=$(cast send \
        --rpc-url "$RPC_URL" \
        --private-key "$FACTORY_OWNER_PRIVATE_KEY" \
        --json \
        --create "$initcode")
    if [[ "$(jq -r '.status // empty' <<< "$deployment_receipt")" != "0x1" ]]; then
        echo "Deploying $contract runtime source failed" >&2
        exit 1
    fi
    source=$(jq -r '.contractAddress // empty' <<< "$deployment_receipt")
    if [[ -z "$source" || "$source" == "null" ]]; then
        echo "Could not read $contract source address from deployment receipt" >&2
        exit 1
    fi

    source_runtime=$(cast code "$source" --rpc-url "$RPC_URL")
    source_hash=$(cast keccak "$source_runtime" | tr '[:upper:]' '[:lower:]')
    if [[ "$source_hash" != "${desired_hashes[$index]}" ]]; then
        echo "Deployed $contract source hash $source_hash does not match ${desired_hashes[$index]}" >&2
        exit 1
    fi

    echo "Installing $contract runtime from $source..."
    update_receipt=$(cast send \
        "$FACTORY_ADDRESS" \
        "${setter}(address)" \
        "$source" \
        --rpc-url "$RPC_URL" \
        --private-key "$FACTORY_OWNER_PRIVATE_KEY" \
        --json)
    if [[ "$(jq -r '.status // empty' <<< "$update_receipt")" != "0x1" ]]; then
        echo "Installing $contract runtime failed" >&2
        exit 1
    fi

    installed_runtime=$(cast code "$target" --rpc-url "$RPC_URL")
    installed_hash=$(cast keccak "$installed_runtime" | tr '[:upper:]' '[:lower:]')
    if [[ "$installed_hash" != "${desired_hashes[$index]}" ]]; then
        echo "Installed $contract hash $installed_hash does not match ${desired_hashes[$index]}" >&2
        exit 1
    fi

    echo "Updated $contract at $target to $installed_hash"
    if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
        echo "- Updated $contract at \`$target\` to \`$installed_hash\`." >> "$GITHUB_STEP_SUMMARY"
    fi
done
