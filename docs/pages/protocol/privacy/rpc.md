# Zone RPC Specification (Draft)

This document specifies the RPC interface for Tempo privacy zones. The design starts from the standard Ethereum JSON-RPC and restricts it to enforce privacy guarantees: callers can only observe state and history relevant to their own account.

## Goals

- Expose a familiar Ethereum JSON-RPC so that existing tooling (wallets, SDKs, block explorers) can connect with minimal changes.
- Authenticate every request via a short-lived access key tied to an Ethereum account.
- Ensure that no RPC call leaks information about accounts other than the caller's own.
- Work in concert with the [execution environment modifications](./execution) that enforce privacy at the EVM level.

## Non-goals

- Public block explorers or permissionless indexing. The zone is a private validium; only authenticated participants and the sequencer can observe state.
- Support for arbitrary smart contract deployment. Zones run a fixed set of predeploys; contract creation is disabled.

## Access keys

Every RPC request must include an access key in the `X-Access-Key` HTTP header (or as the first element of a JSON-RPC batch params wrapper — see [Transport](#transport)). The access key proves that the caller controls a Tempo account and scopes all responses to that account.

Tempo accounts support multiple signature types (secp256k1, P256, and WebAuthn), so the access key scheme must support all of them. Additionally, accounts that have authorized access keys via the [AccountKeychain](/protocol/transactions/AccountKeychain) precompile can use those access keys to authenticate to the RPC on behalf of their root account.

### Message

The signed message is the `keccak256` hash of the following packed encoding:

```solidity
bytes32 accessKeyHash = keccak256(abi.encodePacked(
    bytes32(0x54656d706f5a6f6e65525043),  // "TempoZoneRPC" magic prefix
    uint64(zoneId),                         // zone this key is valid for
    uint64(chainId),                        // zone chain ID (replay protection)
    address(zonePortal),                    // ZonePortal address on Tempo
    uint64(issuedAt),                       // unix timestamp (seconds) of issuance
    uint64(expiresAt)                       // unix timestamp (seconds) of expiry
));
```

This hash is the challenge that must be signed. Using a raw hash (rather than EIP-712 typed data) allows all Tempo signature types to sign the same message consistently.

### Signature types

The access key signature follows the same format as [Tempo transaction signatures](/protocol/transactions/spec-tempo-transaction#signature-types). The signature type is determined by the first byte and length:

| Type | Detection | Address derivation |
|------|-----------|-------------------|
| **secp256k1** | Exactly 65 bytes, no type prefix | `address(uint160(uint256(keccak256(abi.encode(x, y)))))` — standard `ecrecover` |
| **P256** | First byte `0x01`, 130 bytes total | `address(uint160(uint256(keccak256(abi.encodePacked(pubKeyX, pubKeyY)))))` — public key is embedded in the signature |
| **WebAuthn** | First byte `0x02`, variable length (max 2KB) | Same as P256 (WebAuthn uses P256 keys) |
| **Keychain** | First byte `0x03`, variable length | Authenticated account is the `user_address` field, not the signing key's address (see [Keychain access keys](#keychain-access-keys)) |

#### secp256k1

Standard Ethereum signature. The RPC server recovers the public key via `ecrecover(accessKeyHash, v, r, s)` and derives the account address.

#### P256

The signature includes the public key coordinates, so the server verifies the P256 signature directly and derives the address from the embedded public key. If the `pre_hash` flag is set, the server applies `sha256(accessKeyHash)` before verification.

#### WebAuthn

The access key hash is embedded as the WebAuthn challenge (Base64URL-encoded). Verification follows the same rules as [Tempo transaction signatures](/protocol/transactions/spec-tempo-transaction#webauthn-signatures):

**Verified:**

- `authenticatorData` minimum length (37 bytes: 32-byte rpIdHash + 1-byte flags + 4-byte signCount).
- User Presence (UP) or User Verification (UV) flag is set (at least one required).
- AT (attested credential data) flag is NOT set (must be an assertion, not a registration).
- ED (extension data) flag is NOT set (extensions are not supported).
- `clientDataJSON.type` equals `"webauthn.get"`.
- `clientDataJSON.challenge` matches `accessKeyHash` (Base64URL-encoded, no padding).
- P256 signature over `sha256(authenticatorData || sha256(clientDataJSON))` is valid.

**Intentionally skipped:**

- **RP ID hash** (`authenticatorData` bytes 0–31): Not validated. In traditional WebAuthn, the server checks that `rpIdHash` matches its own domain to prevent cross-site credential use. Tempo has no single relying party — users interact through many frontends (wallets, dApps, SDKs), each with a different RP ID. The challenge binding to `accessKeyHash` provides the security guarantee instead: even if a signature is obtained from an unexpected origin, it is only valid for the specific access key parameters that were signed.
- **`clientDataJSON.origin`**: Not validated, for the same reason — there is no canonical origin to check against.
- **Signature counter**: Not checked. Anti-cloning detection is left to the application layer.

The account address is derived from the public key in the signature, following the same derivation as P256.

#### Keychain access keys

Accounts that have authorized access keys via the [AccountKeychain](/protocol/transactions/AccountKeychain) precompile can use those keys to authenticate to the RPC. A Keychain signature wraps an inner signature (secp256k1, P256, or WebAuthn) and includes the root account address:

```
keychain_signature = 0x03 || user_address (20 bytes) || inner_signature
```

The server:

1. Verifies the inner signature against `accessKeyHash`.
2. Derives the signing key's address from the inner signature.
3. Queries the zone's `AccountKeychain` precompile to verify that `user_address` has authorized the signing key (i.e., `getKey(user_address, keyId)` returns an active, non-expired, non-revoked key).
4. Sets the authenticated account to `user_address` (the root account), not the access key's address.

This allows session keys and scoped access keys to authenticate to the RPC with the same permissions as the root account. The AccountKeychain's spending limits and expiry are orthogonal to the RPC access key — the Keychain key must be active to authenticate, but its token-spending limits only apply to on-chain transactions, not RPC reads.

### Validation

The RPC server MUST reject access keys where:

- `expiresAt - issuedAt > 1800` (maximum validity window is 30 minutes).
- `expiresAt <= now` (key has expired).
- `issuedAt > now + 60` (clock skew tolerance of 60 seconds into the future).
- The signature is malformed or does not verify.
- For Keychain signatures: the signing key is not authorized, is revoked, or is expired in the AccountKeychain.

### Sequencer access

The zone sequencer is identified by the `sequencer` address registered in the `ZonePortal` on Tempo. When the authenticated account equals the sequencer address, all restrictions are lifted: the sequencer has universal read access to all state, transactions, and events. This is necessary for the sequencer to produce blocks and batches.

### Transport

Access keys are sent as an HTTP header on every request:

```
POST /
X-Access-Key: <hex-encoded access key>
Content-Type: application/json

{"jsonrpc":"2.0","method":"eth_getBalance","params":[...],"id":1}
```

The `X-Access-Key` value is a single hex-encoded blob containing the concatenation of the signature and the access key fields:

```
<signature bytes><zoneId: 8 bytes><chainId: 8 bytes><zonePortal: 20 bytes><issuedAt: 8 bytes><expiresAt: 8 bytes>
```

The signature portion is variable-length and self-describing (secp256k1 is always 65 bytes; P256/WebAuthn/Keychain signatures start with a type byte). The RPC server parses the signature first using the same detection rules as Tempo transaction signatures, then reads the fixed-size access key fields from the remaining bytes.

Requests without a valid access key receive a `401 Unauthorized` HTTP response. Requests with an expired or malformed key receive `403 Forbidden`.

## RPC method access control

The zone RPC starts from the standard Ethereum JSON-RPC method set and applies per-method restrictions. Each method falls into one of four categories: **allowed** (unrestricted), **scoped** (filtered to the authenticated account), **restricted** (sequencer-only), or **disabled** (not available).

### Allowed methods

These methods return public zone information and are available to any authenticated caller.

| Method | Notes |
|--------|-------|
| `eth_chainId` | Returns the zone's chain ID |
| `eth_blockNumber` | Returns the latest block number |
| `eth_gasPrice` | Returns the current gas price |
| `eth_maxPriorityFeePerGas` | Returns the current priority fee |
| `eth_feeHistory` | Fee history is public |
| `eth_getBlockByNumber` | Returns block headers **without transaction details** (see [Block responses](#block-responses)) |
| `eth_getBlockByHash` | Returns block headers **without transaction details** |
| `eth_subscribe("newHeads")` | Pushes block headers on new blocks. The `logsBloom` field is zeroed (see [Block responses](#block-responses)). |
| `net_version` | Network ID |
| `net_listening` | Node status |
| `web3_clientVersion` | Client version |

### Scoped methods

These methods are available to any authenticated caller but filter results to only include data relevant to the authenticated account.

#### State queries

| Method | Scoping rule |
|--------|-------------|
| `eth_getBalance` | Returns the native balance for the authenticated account only. Queries for other accounts return `0x0`. |
| `eth_getTransactionCount` | Returns the nonce for the authenticated account only. Queries for other accounts return `0x0`. |
| `eth_call` | Executes the call with `from` set to the authenticated account. The [execution environment](./execution) enforces `balanceOf` access control at the EVM level, so callers can only query their own balance. Calls that attempt to read other accounts' state will revert. |
| `eth_estimateGas` | Only allowed when `from` equals the authenticated account. TIP-20 transfers always return the [fixed 100,000 gas](./execution#fixed-gas-constant-transfer-cost). |

#### Transaction access

| Method | Scoping rule |
|--------|-------------|
| `eth_getTransactionByHash` | Returns the transaction only if the authenticated account is the `from` (sender) of the transaction. Returns `null` for transactions sent by other accounts. |
| `eth_getTransactionReceipt` | Returns the receipt only if the authenticated account is the sender. Returns `null` otherwise. Logs within the receipt are filtered (see [Event filtering](#event-filtering)). |
| `eth_sendRawTransaction` | Accepts and broadcasts the transaction. The zone validates that the transaction sender matches the authenticated account. Transactions from mismatched accounts are rejected with error code `-32003` (transaction rejected). |

#### Transaction simulation

| Method | Scoping rule |
|--------|-------------|
| `eth_call` | The `from` field MUST equal the authenticated account. If `from` is omitted, the RPC server sets it to the authenticated account. If `from` is present and does not match, the call is rejected. |
| `eth_estimateGas` | Same restriction as `eth_call`: only the authenticated account can simulate transactions. |
| `eth_createAccessList` | Same restriction: `from` must be the authenticated account. |

**Rationale**: Transaction simulation could reveal state about other accounts (e.g., simulating a transfer to probe whether a recipient exists). Restricting simulation to the caller's own transactions prevents this.

#### Timing side channels and the 100 ms speed bump

Several scoped methods must fetch data from the database before the server can determine whether the authenticated account is authorized to see it. The time difference between "data does not exist" (fast) and "data exists but belongs to another account" (slow fetch, then return `null`) leaks whether the queried object exists at all. For example, `eth_getTransactionByHash` with an unknown hash returns `null` immediately, but the same method with a valid hash belonging to another user takes longer to fetch the transaction, check the sender, and then return `null`.

To close this side channel, the RPC server MUST enforce a **minimum response time of 100 ms** on the following methods:

| Method | Why it needs the speed bump |
|--------|-----------------------------|
| `eth_getTransactionByHash` | Must fetch the transaction to check if `from` matches the caller. Timing leaks whether the tx hash is valid. |
| `eth_getTransactionReceipt` | Must fetch the receipt to check the sender. Same timing leak as above. |
| `eth_getLogs` | Must query logs then post-filter by account. Response time correlates with total log volume, not just the caller's logs. |
| `eth_getFilterLogs` | Same as `eth_getLogs`. |
| `eth_getFilterChanges` | Same as `eth_getLogs`. |

**Implementation**: The server records the wall-clock time at the start of request processing. After computing the response, if less than 100 ms have elapsed, the server sleeps for the remainder before sending the response. This applies regardless of whether the response is populated or empty. The 100 ms floor is chosen to be comfortably above the worst-case database lookup time while remaining imperceptible to interactive users.

Methods that do **not** need the speed bump include those where authorization can be checked before any data fetch:
- `eth_getBalance` / `eth_getTransactionCount`: The server checks if the queried address matches the caller *before* reading state. Non-matching addresses return `0x0` without a lookup.
- `eth_call` / `eth_estimateGas` / `eth_createAccessList`: The `from` field is validated against the authenticated account before execution begins.
- `eth_sendRawTransaction`: Sender verification happens during transaction decoding, before any state access.

#### Event filtering

| Method | Scoping rule |
|--------|-------------|
| `eth_getLogs` | Filtered to only return **TIP-20 events** where the authenticated account is a relevant party. See [Event filtering rules](#event-filtering-rules) below. |
| `eth_getFilterLogs` | Same filtering as `eth_getLogs`. |
| `eth_getFilterChanges` | Same filtering. Only returns new events matching the filter since last poll. |
| `eth_newFilter` | Creates a filter. The filter is implicitly scoped to the authenticated account. |
| `eth_subscribe("logs")` | WebSocket equivalent of `eth_newFilter` + `eth_getFilterChanges`. The subscription is implicitly scoped to the authenticated account using the same [event filtering rules](#event-filtering-rules). |
| `eth_newBlockFilter` | Allowed. Returns new block hashes. |
| `eth_uninstallFilter` | Allowed. Removes a previously created filter. |

### Restricted methods (sequencer-only)

These methods are only available when the authenticated account is the sequencer.

| Method | Notes |
|--------|-------|
| `eth_getCode` | Zone predeploys are precompiles (no EVM bytecode); user accounts never have code. Raw code inspection has no legitimate non-sequencer use case. |
| `eth_getStorageAt` | Raw storage reads bypass all access control. A caller could read TIP-20 balance mappings directly, defeating [execution-level `balanceOf` restrictions](./execution#balance-privacy-balanceof-access-control), or probe account existence via nonce/storage slots. |
| `eth_getBlockByNumber` (with `true`) | Full block with all transactions — sequencer only |
| `eth_getBlockByHash` (with `true`) | Full block with all transactions — sequencer only |
| `eth_getBlockTransactionCountByNumber` | Transaction counts reveal activity levels |
| `eth_getBlockTransactionCountByHash` | Same as above |
| `eth_getTransactionByBlockNumberAndIndex` | Arbitrary transaction access — sequencer only |
| `eth_getTransactionByBlockHashAndIndex` | Same as above |
| `eth_getUncleCountByBlockNumber` | Sequencer only (always returns 0, but restricted for consistency) |
| `eth_getUncleCountByBlockHash` | Same as above |
| `debug_*` | All debug namespace methods |
| `admin_*` | All admin namespace methods |
| `txpool_*` | Transaction pool inspection |

### Disabled methods

These methods are not supported on privacy zones.

| Method | Reason |
|--------|--------|
| `eth_getUncleByBlockNumberAndIndex` | Zones have no uncles |
| `eth_getUncleByBlockHashAndIndex` | Zones have no uncles |
| `eth_mining` | Zones have no mining |
| `eth_hashrate` | Zones have no mining |
| `eth_getWork` | Zones have no mining |
| `eth_submitWork` | Zones have no mining |
| `eth_submitHashrate` | Zones have no mining |
| `eth_getProof` | State proofs could leak information about other accounts' storage layout |
| `eth_getFilterLogs` (unscoped) | All log access goes through the scoped path |
| `eth_subscribe("newPendingTransactions")` | Mempool observation reveals all pending activity. Other subscription types (`newHeads`, `logs`) are classified above. |

Disabled methods return error code `-32601` (method not found).

## Block responses

Block responses are modified to protect transaction privacy:

### Non-sequencer callers

When `eth_getBlockByNumber`, `eth_getBlockByHash`, or `eth_subscribe("newHeads")` returns a block header to a non-sequencer:

- The `transactions` field is **always an empty array** `[]`, regardless of the `include_transactions` parameter. If the parameter is `true`, the request is rejected (sequencer-only).
- The `logsBloom` field MUST be replaced with the zero Bloom (`0x` followed by 512 zero bytes). The Bloom filter is a compressed summary of all log topics and emitting addresses in the block — returning the real value would allow any caller to probe whether a specific address had activity in that block, completely defeating per-account event scoping. The zeroed `logsBloom` is still present in the response for schema compatibility.
- All other header fields (`number`, `hash`, `parentHash`, `timestamp`, `stateRoot`, `transactionsRoot`, `receiptsRoot`, `gasUsed`, `gasLimit`, `baseFeePerGas`, `extraData`) are returned normally.

**Rationale**: Transaction ordering and per-address activity within a block reveals information that could allow correlation attacks. Aggregate activity metrics (`gasUsed`, `gasLimit`) are intentionally public — the zone does not attempt to hide overall transaction volume, only per-account details.

### Sequencer callers

The sequencer receives full block data including all transactions and receipts.

## Event filtering rules

Event filtering is the primary mechanism for users to discover activity relevant to their account. All log queries are restricted to TIP-20 events on the zone token contract.

### Permitted events

Only the following TIP-20 events can be returned by `eth_getLogs` and related methods:

| Event | Signature | Relevant if |
|-------|-----------|-------------|
| `Transfer` | `Transfer(address indexed from, address indexed to, uint256 amount)` | `from == caller` OR `to == caller` |
| `Approval` | `Approval(address indexed owner, address indexed spender, uint256 amount)` | `owner == caller` OR `spender == caller` |
| `TransferWithMemo` | `TransferWithMemo(address indexed from, address indexed to, uint256 amount, bytes32 indexed memo)` | `from == caller` OR `to == caller` |
| `Mint` | `Mint(address indexed to, uint256 amount)` | `to == caller` |
| `Burn` | `Burn(address indexed from, uint256 amount)` | `from == caller` |

All other event topics are filtered out, including system events (`DepositProcessed`, `BatchFinalized`, etc.), role events, and configuration events.

### Filter enforcement

When a user creates a log filter or calls `eth_getLogs`:

1. **Address restriction**: The `address` filter parameter MUST be the zone token address, or omitted (in which case only the zone token address is matched). Filters specifying any other address return empty results.

2. **Topic injection**: The RPC server appends a topic filter that restricts indexed address parameters to the authenticated account. For example, a `Transfer` event filter is automatically constrained so that `topic1` (from) or `topic2` (to) matches the caller's address.

3. **Post-filtering**: After retrieving matching logs, the server performs a final pass to remove any log where the authenticated account is not a relevant party per the table above.

### Example

An authenticated user (address `0xAlice`) calls:

```json
{
  "method": "eth_getLogs",
  "params": [{
    "fromBlock": "0x1",
    "toBlock": "latest",
    "topics": [
      "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"
    ]
  }]
}
```

This requests all `Transfer` events. The RPC server returns only `Transfer` events where `from == 0xAlice` or `to == 0xAlice`. Transfer events between two other parties are never returned.

## Zone-specific RPC methods

In addition to the Ethereum JSON-RPC methods, the zone exposes zone-specific methods under the `zone_` namespace.

| Method | Access | Description |
|--------|--------|-------------|
| `zone_getAccessKeyInfo` | Any authenticated | Returns the authenticated account address and key expiry. Useful for verifying the access key is valid. |
| `zone_getZoneInfo` | Any authenticated | Returns zone metadata: `zoneId`, `zoneToken`, `sequencer` (address only, not private key), `chainId`. |
| `zone_getDepositStatus(tempoBlockNumber)` | Scoped | Returns whether deposits from the given Tempo block have been processed on the zone. Only returns information about deposits where the sender or recipient is the authenticated account. |

**Withdrawals**: To request a withdrawal, the caller MUST construct and sign a transaction calling `ZoneOutbox.requestWithdrawal(...)` and submit it via `eth_sendRawTransaction`. There is no server-side convenience method — access keys are read-only credentials and MUST NOT be sufficient to authorize state-changing operations such as token transfers or withdrawals. Requiring a full transaction signature ensures that a stolen or replayed access key cannot be used to move funds.

## Error codes

In addition to standard JSON-RPC error codes, the zone RPC uses:

| Code | Message | Meaning |
|------|---------|---------|
| `-32001` | `Access key required` | No access key provided |
| `-32002` | `Access key expired` | The access key has expired |
| `-32003` | `Transaction rejected` | Transaction sender does not match authenticated account |
| `-32004` | `Account mismatch` | The queried account does not match the authenticated account |
| `-32005` | `Sequencer only` | Method requires sequencer access |
| `-32006` | `Method disabled` | Method is not available on privacy zones |

## Security considerations

- **Side channels via timing**: Scoped methods that must fetch data before checking authorization are subject to a mandatory 100 ms response floor (see [Timing side channels and the 100 ms speed bump](#timing-side-channels-and-the-100-ms-speed-bump)). This ensures that `eth_getTransactionByHash` for a non-existent transaction and for another user's transaction have indistinguishable response times.
- **Nonce privacy**: `eth_getTransactionCount` for non-authenticated accounts returns `0x0` rather than an error. This avoids revealing whether an account exists. The constant `0x0` response is indistinguishable from a genuinely new account.
- **Access key replay**: Access keys are scoped to a specific zone (`zoneId` and `chainId`) and a specific portal (`zonePortal`), with a maximum 30-minute window. Access keys are strictly read-only credentials — no RPC method that is authenticated solely by an access key may modify state (see [Withdrawals](#zone-specific-rpc-methods)). The RPC server SHOULD implement nonce tracking or rate limiting to further reduce the window for abuse if a key is intercepted, but replay of a read-only key cannot move funds.
- **Keychain key revocation**: When a Keychain access key is used, the RPC server verifies the key's status against the AccountKeychain precompile on every request (or caches with a short TTL). If a root account revokes an access key on-chain, the RPC SHOULD stop accepting that key within a reasonable window. Implementations SHOULD cache Keychain key status for at most one zone block to balance performance and revocation latency.
- **P256/WebAuthn key compromise**: Unlike secp256k1, P256 and WebAuthn keys include the public key in the signature. This means the public key is visible to the RPC server on every request. This is not a security concern (public keys are public), but implementations should be aware that the key material is transmitted in the clear over the connection.
- **Metadata leakage**: Even with content-level privacy, connection-level metadata (IP addresses, request timing, request frequency) can leak information. Deployments SHOULD use TLS and MAY require additional transport-level privacy measures.
- **Fixed gas and transfer receipts**: The [fixed 100,000 gas cost](./execution#fixed-gas-constant-transfer-cost) on TIP-20 transfers ensures that `gasUsed` in transaction receipts is identical for all transfers. Without this, an observer who obtains a receipt (e.g., the sender) could infer whether the recipient was a new or existing account.
- **Block header sanitization**: Block headers returned to non-sequencer callers have `logsBloom` zeroed (see [Block responses](#block-responses)). The Bloom filter would otherwise allow probing whether a specific address had activity in a given block, defeating per-account event scoping. This applies to all code paths that return block headers: `eth_getBlockByNumber`, `eth_getBlockByHash`, and `eth_subscribe("newHeads")`. Aggregate fields like `gasUsed` are intentionally public — the zone does not hide overall activity volume, only per-account details.

## Implementation notes

- The zone node enforces access control at two layers: the RPC server (request filtering) and the [EVM execution environment](./execution) (TIP-20 modifications). Both layers are required — see [Interaction with RPC](./execution#interaction-with-rpc) for why neither layer alone is sufficient.
- Filter state (from `eth_newFilter`) is stored per-authenticated-account. Filters created by one access key are accessible by subsequent access keys for the same account.
- The zone node SHOULD cache access key verification results for the duration of the key's validity to avoid repeated signature recovery. For Keychain access keys, the AccountKeychain state should be cached with a short TTL (e.g., one zone block) since key revocation can happen on-chain at any time.
- P256 and WebAuthn signature verification is more expensive than secp256k1. The RPC server SHOULD aggressively cache verified access keys to amortize the verification cost. A verified access key can be cached by its hash for the remaining duration of its validity.
- WebSocket connections (`eth_subscribe`) follow the same access key model. The access key is provided during the WebSocket handshake and scopes all subscriptions for that connection. The connection is terminated when the access key expires; clients must reconnect with a fresh key.
