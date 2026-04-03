# Zone RPC Specification (Draft)

This document specifies the RPC interface for Tempo privacy zones. The design starts from the standard Ethereum JSON-RPC and restricts it to enforce privacy guarantees: callers can only observe state and history relevant to their own account.

## Goals

- Expose a familiar Ethereum JSON-RPC so that existing tooling (wallets, SDKs, block explorers) can connect with minimal changes.
- Authenticate every request via a short-lived authorization token tied to an Ethereum account.
- Ensure that no RPC call leaks information about accounts other than the caller's own.
- Work in concert with the [execution environment modifications](./execution) that enforce privacy at the EVM level.

## Non-goals

- Public block explorers or permissionless indexing. The zone is a private validium; only authenticated participants and the sequencer can observe state.
- Support for arbitrary smart contract deployment. Zones run a fixed set of predeploys; contract creation is disabled.

## Authorization tokens

Every RPC request must include an authorization token in the `X-Authorization-Token` HTTP header (or as the first element of a JSON-RPC batch params wrapper — see [Transport](#transport)). The authorization token proves that the caller controls a Tempo account and scopes all responses to that account.

Tempo accounts support multiple signature types (secp256k1, P256, and WebAuthn), so the authorization token scheme must support all of them. Additionally, accounts that have authorized Access Keys via the [AccountKeychain](/protocol/transactions/AccountKeychain) precompile can use those Access Keys to authenticate to the RPC on behalf of their root account. The RPC server uses the same signature parser and recovery rules as Tempo transactions, so auth tokens accept the same wire formats as transaction signatures.

### Message

The signed message is the `keccak256` hash of the following packed encoding:

```solidity
bytes32 authorizationTokenHash = keccak256(abi.encodePacked(
    bytes32(0x54656d706f5a6f6e65525043),  // "TempoZoneRPC" magic prefix
    uint8(version),                         // spec version (currently 0)
    uint32(zoneId),                         // zone this key is valid for
    uint64(chainId),                        // zone chain ID (replay protection)
    address(zonePortal),                    // ZonePortal address on Tempo
    uint64(issuedAt),                       // unix timestamp (seconds) of issuance
    uint64(expiresAt)                       // unix timestamp (seconds) of expiry
));
```

The `version` field MUST be `0` for this version of the spec. The RPC server MUST reject authorization tokens with an unrecognized version. This allows future revisions to change authorization token semantics (e.g., adding scoped permissions) without ambiguity.

This hash is the challenge that must be signed. Using a raw `keccak256` hash (rather than EIP-191 personal messages or EIP-712 typed data) allows all Tempo signature types to sign the same message consistently — P256 and WebAuthn signers cannot produce EIP-191 prefixed signatures. The `"TempoZoneRPC"` magic prefix provides domain separation, ensuring that authorization token hashes cannot collide with Tempo transaction hashes or other signing contexts.

### Signature types

The authorization token signature follows the same format as [Tempo transaction signatures](/protocol/transactions/spec-tempo-transaction#signature-types). The signature type is determined by the first byte and length:

| Type | Detection | Address derivation |
|------|-----------|-------------------|
| **secp256k1** | Exactly 65 bytes, no type prefix | `address(uint160(uint256(keccak256(abi.encode(x, y)))))` — standard `ecrecover` |
| **P256** | First byte `0x01`, 130 bytes total | `address(uint160(uint256(keccak256(abi.encodePacked(pubKeyX, pubKeyY)))))` — public key is embedded in the signature |
| **WebAuthn** | First byte `0x02`, variable length (max 2KB) | Same as P256 (WebAuthn uses P256 keys) |
| **Keychain** | First byte `0x03` (legacy V1) or `0x04` (V2), variable length | Authenticated account is the `user_address` field, not the signing key's address (see [Keychain Access Keys](#keychain-access-keys)) |

#### secp256k1

Standard Ethereum signature. The RPC server recovers the public key via `ecrecover(authorizationTokenHash, v, r, s)` and derives the account address.

#### P256

The signature includes the public key coordinates, so the server verifies the P256 signature directly and derives the address from the embedded public key. If the `pre_hash` flag is set, the server applies `sha256(authorizationTokenHash)` before verification.

#### WebAuthn

The authorization token hash is embedded as the WebAuthn challenge (Base64URL-encoded). Verification follows the same rules as [Tempo transaction signatures](/protocol/transactions/spec-tempo-transaction#webauthn-signatures):

**Verified:**

- `authenticatorData` minimum length (37 bytes: 32-byte rpIdHash + 1-byte flags + 4-byte signCount).
- User Presence (UP) or User Verification (UV) flag is set (at least one required).
- AT (attested credential data) flag is NOT set (must be an assertion, not a registration).
- ED (extension data) flag is NOT set (extensions are not supported).
- `clientDataJSON.type` equals `"webauthn.get"`.
- `clientDataJSON.challenge` matches `authorizationTokenHash` (Base64URL-encoded, no padding).
- P256 signature over `sha256(authenticatorData || sha256(clientDataJSON))` is valid.

**Intentionally skipped:**

- **RP ID hash** (`authenticatorData` bytes 0–31): Not validated. In traditional WebAuthn, the server checks that `rpIdHash` matches its own domain to prevent cross-site credential use. Tempo has no single relying party — users interact through many frontends (wallets, dApps, SDKs), each with a different RP ID. The challenge binding to `authorizationTokenHash` provides the security guarantee instead: even if a signature is obtained from an unexpected origin, it is only valid for the specific authorization token parameters that were signed.
- **`clientDataJSON.origin`**: Not validated, for the same reason — there is no canonical origin to check against.
- **Signature counter**: Not checked. Anti-cloning detection is left to the application layer.

The account address is derived from the public key in the signature, following the same derivation as P256.

#### Keychain Access Keys

Accounts that have authorized Access Keys via the zone's `AccountKeychain` precompile can use those keys to authenticate to the RPC. The zone has its own independent `AccountKeychain` instance — it is **not** mirrored from Tempo L1. Users must register Keychain keys on the zone directly via transactions submitted to the zone's `AccountKeychain` precompile. This means a key registered on Tempo L1 does not automatically grant RPC access to the zone; the user must separately authorize it on the zone.

A Keychain signature wraps an inner signature (secp256k1, P256, or WebAuthn) and includes the root account address. The RPC server accepts both the legacy V1 and current V2 encodings supported by Tempo transaction signatures:

```
keychain_signature_v1 = 0x03 || user_address (20 bytes) || inner_signature
keychain_signature_v2 = 0x04 || user_address (20 bytes) || inner_signature
```

V2 is recommended. V2 binds `user_address` into the access-key signing hash, while V1 signs the raw authorization-token hash directly for backward compatibility.

The server:

1. Parses the signature using the same rules as Tempo transactions.
2. Verifies the inner signature against `authorizationTokenHash` for V1, or against `keccak256(0x04 || authorizationTokenHash || user_address)` for V2.
3. Derives the signing key's address from the inner signature.
4. Queries the zone's `AccountKeychain` precompile to verify that `user_address` has authorized the signing key (i.e., `getKey(user_address, keyId)` returns an active, non-expired, non-revoked key).
5. Verifies that the stored `signatureType` matches the inner signature type (secp256k1, P256, or WebAuthn).
6. Sets the authenticated account to `user_address` (the root account), not the Access Key's address.

This allows session keys and scoped Access Keys to authenticate to the RPC with the same permissions as the root account. The AccountKeychain's spending limits and expiry are orthogonal to the RPC authorization token — the Keychain key must be active to authenticate, but its token-spending limits only apply to on-chain transactions, not RPC reads.

### Validation

The RPC server MUST reject authorization tokens where:

- `zoneId` does not equal the zone's configured `zoneId`.
- `chainId` does not equal the zone's configured chain ID (`eth_chainId`).
- `zonePortal` does not equal the zone's configured Tempo ZonePortal address.
- `expiresAt - issuedAt > 1800` (maximum validity window is 30 minutes).
- `expiresAt <= now` (key has expired).
- `issuedAt > now + 60` (clock skew tolerance of 60 seconds into the future).
- The signature is malformed or does not verify.
- For Keychain signatures: the signing key is not authorized, is revoked, or is expired in the AccountKeychain.

### Sequencer access

The zone sequencer is identified by the `sequencer` address registered in the `ZonePortal` on Tempo. When the authenticated account equals the sequencer address, all restrictions are lifted: the sequencer has universal read access to all state, transactions, and events. This is necessary for the sequencer to produce blocks and batches.

### Transport

Authorization tokens are sent as an HTTP header on every request:

```
POST /
X-Authorization-Token: <hex-encoded authorization token>
Content-Type: application/json

{"jsonrpc":"2.0","method":"eth_getBalance","params":[...],"id":1}
```

The `X-Authorization-Token` value is a single hex-encoded blob containing the concatenation of the signature and the authorization token fields:

```
<signature bytes><version: 1 byte><zoneId: 4 bytes><chainId: 8 bytes><zonePortal: 20 bytes><issuedAt: 8 bytes><expiresAt: 8 bytes>
```

The authorization token fields are always exactly 49 bytes (1 + 4 + 8 + 20 + 8 + 8). To parse the blob, the RPC server reads the **last 49 bytes** as the authorization token fields, and treats everything before them as the signature. The signature is then parsed using the same detection rules as [Tempo transaction signatures](/protocol/transactions/spec-tempo-transaction#signature-types) (secp256k1 is exactly 65 bytes; P256 starts with `0x01` and is 130 bytes; WebAuthn starts with `0x02` and is variable-length; Keychain starts with `0x03` for legacy V1 or `0x04` for V2 and is variable-length). Parsing from the end avoids any ambiguity with variable-length signature types.

Requests without an authorization token receive a `401 Unauthorized` HTTP response. Requests with an expired, malformed, or unauthorized token receive `403 Forbidden`. If Keychain token verification fails because the server cannot read `AccountKeychain.getKey(...)`, the server returns `500 Internal Server Error`.

## RPC method access control

The zone RPC starts from the standard Ethereum JSON-RPC method set and applies per-method restrictions. Each method falls into one of four categories: **allowed** (unrestricted), **scoped** (filtered to the authenticated account), **restricted** (sequencer-only), or **disabled** (not available).

**Default deny**: Any method not explicitly listed below MUST return error code `-32601` (method not found). This ensures that new methods added by future Ethereum specs or node implementations are not accidentally exposed without privacy review.

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
| `eth_syncing` | Returns sync status. Low risk — reveals node state, not account data. |
| `eth_coinbase` | Returns the sequencer address (already public via `zone_getZoneInfo`). |
| `net_version` | Network ID |
| `net_listening` | Node status |
| `web3_clientVersion` | Client version |
| `web3_sha3` | Pure Keccak-256 hash — no state access |

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
| `eth_call` | The `from` field MUST equal the authenticated account. If `from` is omitted, the RPC server sets it to the authenticated account. If `from` is present and does not match, the call is rejected with error code `-32004` (account mismatch). Requests from non-sequencer callers that include a state override set or block override object (client-specific simulation extensions) MUST be rejected with `-32602` (invalid params). |
| `eth_estimateGas` | Same restriction as `eth_call`: only the authenticated account can simulate transactions. Returns `-32004` on mismatch. Requests from non-sequencer callers that include a state override set or block override object MUST be rejected with `-32602` (invalid params). |

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
- `eth_call` / `eth_estimateGas`: The `from` field is validated against the authenticated account before execution begins.
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
| `eth_createAccessList` | Returns the set of storage slots a transaction would access. Could reveal storage layout of accounts the transaction interacts with, especially if smart contracts are added in the future. |
| `eth_getCode` | Zone predeploys are precompiles (no EVM bytecode); user accounts never have code. Raw code inspection has no legitimate non-sequencer use case. |
| `eth_getStorageAt` | Raw storage reads bypass all access control. A caller could read TIP-20 balance mappings directly, defeating [execution-level `balanceOf` restrictions](./execution#balance-privacy-balanceof-access-control), or probe account existence via nonce/storage slots. |
| `eth_getBlockByNumber` (with `true`) | Full block with all transactions — sequencer only |
| `eth_getBlockByHash` (with `true`) | Full block with all transactions — sequencer only |
| `eth_getBlockTransactionCountByNumber` | Transaction counts reveal activity levels |
| `eth_getBlockTransactionCountByHash` | Same as above |
| `eth_getTransactionByBlockNumberAndIndex` | Arbitrary transaction access — sequencer only |
| `eth_getTransactionByBlockHashAndIndex` | Same as above |
| `eth_getBlockReceipts` | Returns all receipts in a block, bypassing per-sender receipt scoping |
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
| `eth_getProof` | Merkle proofs include sibling hashes that reveal state trie structure (number of accounts, address prefix distribution, slot occupancy), leaking information beyond the queried account |
| `eth_getFilterLogs` (unscoped) | All log access goes through the scoped path |
| `eth_newPendingTransactionFilter` | Polling equivalent of `eth_subscribe("newPendingTransactions")` — mempool observation |
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
| `zone_getAuthorizationTokenInfo` | Any authenticated | Returns the authenticated account address and token expiry. Useful for verifying the authorization token is valid. |
| `zone_getZoneInfo` | Any authenticated | Returns zone metadata: `zoneId`, `zoneTokens`, `sequencer` (address only, not private key), `chainId`. |
| `zone_getDepositStatus(tempoBlockNumber)` | Scoped | Returns whether deposits from the given Tempo block have been processed on the zone. Only returns information about deposits where the sender or recipient is the authenticated account. |

All integer fields in these responses use Ethereum JSON-RPC quantity encoding (hex strings such as `0x1`).

### `zone_getAuthorizationTokenInfo`

**Request**

```json
{
  "method": "zone_getAuthorizationTokenInfo",
  "params": []
}
```

**Response**

```json
{
  "account": "0x1234...",
  "expiresAt": "0x67d2d7c0"
}
```

- `account`: the authenticated account address recovered from the authorization token.
- `expiresAt`: the token expiry timestamp (unix seconds).

### `zone_getZoneInfo`

**Request**

```json
{
  "method": "zone_getZoneInfo",
  "params": []
}
```

**Response**

```json
{
  "zoneId": "0x1",
  "zoneTokens": [
    "0x20c0000000000000000000000000000000000000",
    "0x20c0000000000000000000000000000000aa0001"
  ],
  "sequencer": "0xabcd...",
  "chainId": "0x2a"
}
```

- `zoneId`: the configured zone identifier.
- `zoneTokens`: the zone's currently enabled TIP-20 token addresses.
- `sequencer`: the configured sequencer address.
- `chainId`: the zone chain ID.

### `zone_getDepositStatus(tempoBlockNumber)`

**Request**

```json
{
  "method": "zone_getDepositStatus",
  "params": ["0x2a"]
}
```

`tempoBlockNumber` MUST be supplied as a JSON-RPC hex quantity string.

**Response**

```json
{
  "tempoBlockNumber": "0x2a",
  "zoneProcessedThrough": "0x2a",
  "processed": true,
  "deposits": [
    {
      "depositHash": "0xfeed...",
      "kind": "regular",
      "token": "0x20c0000000000000000000000000000000000000",
      "sender": "0xaaaa...",
      "recipient": "0xbbbb...",
      "amount": "0xf4240",
      "memo": "0x1111...",
      "status": "processed"
    }
  ]
}
```

- `tempoBlockNumber`: the queried Tempo L1 block number.
- `zoneProcessedThrough`: the latest Tempo block number the zone has processed on L2.
- `processed`: `true` if the zone has advanced through `tempoBlockNumber` and every deposit visible to the authenticated caller from that block has reached a terminal status.
- `deposits`: only deposits relevant to the authenticated caller.

Each deposit entry has:

- `depositHash`: the deposit queue hash (`newCurrentDepositQueueHash`) for that deposit.
- `kind`: `"regular"` or `"encrypted"`.
- `token`: the deposited token address.
- `sender`: the L1 depositor address.
- `recipient`: the plaintext recipient address when visible; otherwise `null`.
- `amount`: the post-fee deposit amount.
- `memo`: the deposit memo when visible; otherwise `null`.
- `status`: `"pending"`, `"processed"`, or `"failed"`.

Visibility rules:

- Regular deposits are returned only when the authenticated account is the sender or the plaintext recipient.
- Encrypted deposits are returned immediately to the sender.
- Encrypted deposits are returned to a recipient-only caller only after the zone has emitted `EncryptedDepositProcessed` on L2, which reveals the recipient.
- Pending encrypted deposits MUST keep `recipient` and `memo` as `null`; implementations MUST NOT decrypt or reveal hidden recipient data just to answer RPC.

**Withdrawals**: To request a withdrawal, the caller MUST construct and sign a transaction calling `ZoneOutbox.requestWithdrawal(...)` and submit it via `eth_sendRawTransaction`. There is no server-side convenience method — authorization tokens are read-only credentials and MUST NOT be sufficient to authorize state-changing operations such as token transfers or withdrawals. Requiring a full transaction signature ensures that a stolen or replayed authorization token cannot be used to move funds.

## Error codes

In addition to standard JSON-RPC error codes, the zone RPC uses:

| Code | Message | Meaning |
|------|---------|---------|
| `-32001` | `Authorization token required` | No authorization token provided |
| `-32002` | `Authorization token expired` | The authorization token has expired |
| `-32003` | `Transaction rejected` | Transaction sender does not match authenticated account (`eth_sendRawTransaction`) |
| `-32004` | `Account mismatch` | The `from` field does not match the authenticated account (`eth_call`, `eth_estimateGas`) |
| `-32005` | `Sequencer only` | Method requires sequencer access |
| `-32006` | `Method disabled` | Method is not available on privacy zones |

**Error vs. silent response**: Methods where the user explicitly provides a mismatched parameter (`eth_sendRawTransaction` with wrong sender, `eth_call` with wrong `from`) return explicit errors — the user already knows the address they supplied, so the error leaks nothing. Methods that query *about* other accounts (`eth_getBalance`, `eth_getTransactionByHash`, etc.) return silent dummy values (`0x0`, `null`, empty results) instead of errors — an error would reveal "this data exists but you can't see it," which leaks information.

## Security considerations

- **Side channels via timing**: Scoped methods that must fetch data before checking authorization are subject to a mandatory 100 ms response floor (see [Timing side channels and the 100 ms speed bump](#timing-side-channels-and-the-100-ms-speed-bump)). This ensures that `eth_getTransactionByHash` for a non-existent transaction and for another user's transaction have indistinguishable response times.
- **Nonce privacy**: `eth_getTransactionCount` for non-authenticated accounts returns `0x0` rather than an error. This avoids revealing whether an account exists. The constant `0x0` response is indistinguishable from a genuinely new account.
- **Authorization token replay**: Authorization tokens are scoped to a specific zone (`zoneId` and `chainId`) and a specific portal (`zonePortal`), with a maximum 30-minute window. Authorization tokens are strictly read-only credentials — no RPC method that is authenticated solely by an authorization token may modify state (see [Withdrawals](#zone-specific-rpc-methods)). The RPC server SHOULD implement nonce tracking or rate limiting to further reduce the window for abuse if a token is intercepted, but replay of a read-only token cannot move funds.
- **Simulation override extensions**: Some Ethereum clients support non-standard simulation extensions (state override sets and block override objects) on `eth_call`/`eth_estimateGas`. Privacy zones MUST reject these extensions for non-sequencer callers, because they can bypass or distort normal state access assumptions used by this spec's privacy model.
- **Keychain key revocation and expiry**: When a root account revokes a Keychain key on-chain, the RPC server MUST stop accepting that key within 1 second of importing the block that contains the revocation. Cached Keychain verifications MUST also honor key expiry: a cache entry MUST expire no later than `min(authorizationToken.expiresAt, keyExpiry)` where `keyExpiry` is read from `AccountKeychain.getKey(...)`. In the current `AccountKeychain` implementation, inactive or revoked keys are surfaced as `keyId == 0` and `expiry == 0`, so those results MUST be treated as immediately invalid rather than "never expires." The recommended implementation is event-driven: the zone node watches for `KeyRevoked(account, publicKey)` events emitted by the AccountKeychain precompile during block execution and immediately evicts matching entries from the authorization token cache. This requires no cryptography — just a cache lookup and delete. As a fallback, implementations MAY poll the precompile via `getKey(user_address, keyId)`, but the 1-second deadline still applies.
- **P256/WebAuthn key compromise**: Unlike secp256k1, P256 and WebAuthn keys include the public key in the signature. This means the public key is visible to the RPC server on every request. This is not a security concern (public keys are public), but implementations should be aware that the key material is transmitted in the clear over the connection.
- **Metadata leakage**: Even with content-level privacy, connection-level metadata (IP addresses, request timing, request frequency) can leak information. Deployments SHOULD use TLS and MAY require additional transport-level privacy measures.
- **Fixed gas and transfer receipts**: The [fixed 100,000 gas cost](./execution#fixed-gas-constant-transfer-cost) on TIP-20 transfers ensures that `gasUsed` in transaction receipts is identical for all transfers. Without this, an observer who obtains a receipt (e.g., the sender) could infer whether the recipient was a new or existing account.
- **Block header sanitization**: Block headers returned to non-sequencer callers have `logsBloom` zeroed (see [Block responses](#block-responses)). The Bloom filter would otherwise allow probing whether a specific address had activity in a given block, defeating per-account event scoping. This applies to all code paths that return block headers: `eth_getBlockByNumber`, `eth_getBlockByHash`, and `eth_subscribe("newHeads")`. Aggregate fields like `gasUsed` are intentionally public — the zone does not hide overall activity volume, only per-account details.

## Implementation notes

- The zone node enforces access control at two layers: the RPC server (request filtering) and the [EVM execution environment](./execution) (TIP-20 modifications). Both layers are required — see [Interaction with RPC](./execution#interaction-with-rpc) for why neither layer alone is sufficient.
- Filter state (from `eth_newFilter`) is stored per-authenticated-account. Filters created by one authorization token are accessible by subsequent authorization tokens for the same account.
- The zone node SHOULD cache authorization token verification results for the duration of token validity to avoid repeated signature recovery. For Keychain Access Keys, cache entries MUST have a TTL bounded by the earlier of authorization-token expiry and the active key's `expiry`, and MUST be invalidated within 1 second of importing a block that revokes the key (see [Keychain key revocation and expiry](#security-considerations)). The recommended approach is to hook into block import and evict cache entries when `KeyRevoked` events are observed.
- P256 and WebAuthn signature verification is more expensive than secp256k1. The RPC server SHOULD aggressively cache verified authorization tokens to amortize the verification cost. A verified authorization token can be cached by its hash for the remaining duration of its validity.
- WebSocket connections (`eth_subscribe`) follow the same authorization-token model. The authorization token is provided during the WebSocket handshake and scopes all subscriptions for that connection. The connection is terminated when the authorization token expires; clients must reconnect with a fresh token. For Keychain-authenticated WebSocket connections, the server MUST also terminate the connection within 1 second of importing a block that revokes the Keychain key, following the same deadline as [Keychain key revocation](#security-considerations).
