# Rollup Execution Layer Specification

This document specifies the **current** behavior of an Optimism-style rollup execution layer (EL) as implemented by op-reth, assuming a **new chain** where the **current rule set is active from genesis**. It intentionally omits any historical upgrades, legacy formats, and deprecated behavior.

The specification focuses on:
- Block and transaction validity rules (consensus-layer validation for EL objects).
- State transition behavior relevant to rollup-specific transaction types and fees.
- Byte-level encoding of serialized data (transactions, receipts, L1 info calldata, header extra data).
- Fee rules: L2 execution fees, L1 data fees, operator fees, and DA-footprint accounting.

Normative keywords: **MUST**, **MUST NOT**, **SHOULD**, **SHOULD NOT**, **MAY**.

## 1. Overview

### 1.1 What is a rollup execution layer?

A rollup execution layer is an Ethereum Virtual Machine (EVM) execution environment that:
- Executes EVM transactions and produces EVM state roots like Ethereum L1.
- Derives part of its economic security and data availability from an L1 chain (the “origin chain”).
- Accepts **deposit** transactions originating from L1 and supports **withdrawals** from L2 to L1 via a message-passing contract.

### 1.2 How it differs from L1 Ethereum

Compared to L1 Ethereum, this rollup EL differs in the following protocol-level ways:
- **A special deposit transaction type (0x7E)** exists. Deposits have no ECDSA signature and are authorized by L1 inclusion, not L2 signatures.
- Every non-genesis L2 block MUST include an **L1 Block Info** system transaction as its first transaction. This transaction updates a predeployed **L1 Block Info contract** whose storage provides parameters used for fee calculation and DA-footprint accounting.
- L2 transactions pay additional fees representing the cost to publish their data on L1, using a **FastLZ-based data-availability estimator**.
- The block header uses the fields `blob_gas_used` and `excess_blob_gas` for **DA-footprint accounting** (not for EIP-4844 blob transactions).
- The block header’s `withdrawals_root` is repurposed to commit to the **storage root of the withdrawal message passer contract**, and the block body withdrawals list is required to be present and empty.

### 1.3 Key concepts

#### 1.3.1 Sequencer

The sequencer is the actor that proposes L2 blocks and provides:
- The ordered list of L2 transactions for the block.
- The first “L1 Block Info” transaction which encodes the L1 origin metadata and fee parameters used by the rollup.

This document specifies **how blocks are validated and executed**, not sequencer selection or leader election.

#### 1.3.2 L1 origin metadata

Each L2 block is associated with an L1 “origin” block. A subset of the L1 origin block fields and rollup-specific parameters are made available on L2 by writing them into the **L1 Block Info contract** via the first transaction in each block.

#### 1.3.3 Deposits

Deposits are transactions forced onto L2 from L1. Deposits can:
- Mint ETH to the depositor (`mint` field).
- Call or create contracts.
- Carry calldata (`input` field).

Deposits are encoded as an EIP-2718 typed transaction with type byte `0x7E`.

#### 1.3.4 Withdrawals

Withdrawals are represented as L2 messages written into a predeployed message passer contract. The L2 block header commits to the **message passer contract’s storage root** using the header `withdrawals_root` field.

## 2. Type Definitions

This section defines the **serialized formats** that are consensus-critical for this rollup EL.

### 2.1 Transaction Types

The execution layer supports the following transaction envelopes:
- Legacy (RLP, no type byte)
- EIP-2930 (typed transaction)
- EIP-1559 (typed transaction)
- EIP-7702 (typed transaction)
- Deposit (typed transaction, type byte `0x7E`)

All typed transactions use EIP-2718 framing:
- The transaction is encoded as `type_byte || payload`
- `type_byte` is one byte.
- `payload` is the transaction-type-specific encoding (RLP list for the types used here).

#### 2.1.1 Common definitions

- **Byte order**: all fixed-width integers encoded in byte arrays are **big-endian**.
- **Address**: 20 bytes.
- **B256 / bytes32**: 32 bytes.
- **u64**: 8-byte unsigned integer.
- **u32**: 4-byte unsigned integer.
- **u16**: 2-byte unsigned integer.
- **bool**: a single byte, `0x00` for false, `0x01` for true.
- **U256**: 0–32 bytes in RLP scalar form (big-endian, no leading zeros), or 32 bytes when specified as `uint256` in calldata / storage packing.

#### 2.1.2 Deposit transaction (type 0x7E)

**Type byte**
- `DEPOSIT_TX_TYPE = 0x7E` (decimal 126).

**Origin**
- A deposit transaction is considered “L1-originated”.
- It is included in an L2 block by the sequencer based on L1 inputs (outside the scope of this spec).

**Signature**
- Deposits have **no ECDSA signature**.
- For sender recovery purposes, the `from` field is the transaction sender.

**Fields**

A deposit transaction has the following logical fields, in this exact order:

1. `source_hash`: bytes32 (32 bytes)
2. `from`: address (20 bytes)
3. `to`: transaction kind
   - **Call**: 20-byte address
   - **Create**: empty byte string (RLP empty string)
4. `mint`: unsigned integer (u128 logical value)
5. `value`: uint256 logical value
6. `gas_limit`: u64
7. `is_system_transaction`: bool (1 byte)
8. `input`: arbitrary bytes

**RLP payload encoding**

The payload is an RLP list with the above fields encoded as follows:
- `source_hash`: RLP string of length 32.
- `from`: RLP string of length 20.
- `to`:
  - Call: RLP string of length 20.
  - Create: RLP empty string (zero-length).
- `mint`: RLP scalar encoding of the u128 value (no leading zeros). The logical value range is 0..2^128-1.
- `value`: RLP scalar encoding (no leading zeros), up to 32 bytes.
- `gas_limit`: RLP scalar encoding (no leading zeros), up to 8 bytes.
- `is_system_transaction`: RLP scalar encoding of a single byte `0x00` or `0x01`.
- `input`: RLP string of length `len(input)`.

**Full EIP-2718 encoding**

The full deposit transaction encoding is:
- `0x7E || rlp([source_hash, from, to, mint, value, gas_limit, is_system_transaction, input])`

**Transaction hash**

The transaction hash is:
- `keccak256( 0x7E || rlp([source_hash, from, to, mint, value, gas_limit, is_system_transaction, input]) )`

**Validity constraints**

- `is_system_transaction` MUST be `0x00` under the current rule set.
- The EL does not validate the derivation of `source_hash`; it is treated as an opaque 32-byte value.

#### 2.1.3 Legacy transaction (untyped)

Legacy transactions are encoded as a single RLP list (no type byte prefix).

**Logical fields**

1. `nonce` (u64)
2. `gas_price` (u128 logical value)
3. `gas_limit` (u64)
4. `to` (transaction kind: address or create marker)
5. `value` (uint256 logical value)
6. `input` (bytes)
7. `v` (signature recovery / chain-id)
8. `r` (ECDSA signature scalar)
9. `s` (ECDSA signature scalar)

**RLP encoding**

The canonical legacy transaction encoding is:

`rlp([nonce, gas_price, gas_limit, to, value, input, v, r, s])`

Encoding rules:
- `nonce`, `gas_price`, `gas_limit`, `value`, `v`, `r`, `s` are RLP scalars.
- `to`:
  - Call: RLP string of length 20 bytes (the destination address)
  - Create: RLP empty string (zero-length)
- `input`: RLP string of length `len(input)`

**Signature hash**

The legacy transaction signature hash is computed according to EIP-155:
- If the transaction carries a chain id, the signing payload is:
  - `rlp([nonce, gas_price, gas_limit, to, value, input, chain_id, 0, 0])`
- Otherwise, the signing payload is:
  - `rlp([nonce, gas_price, gas_limit, to, value, input])`

Then:
- `sig_hash = keccak256(signing_payload)`

#### 2.1.4 EIP-2930 transaction (typed, type 0x01)

EIP-2930 transactions are EIP-2718 typed transactions with type byte `0x01`.

**RLP payload field order (unsigned fields)**

`[chain_id, nonce, gas_price, gas_limit, to, value, input, access_list]`

**Signature fields**

The signed transaction appends:
- `y_parity` (0 or 1)
- `r`
- `s`

**Full EIP-2718 encoding**

`0x01 || rlp([chain_id, nonce, gas_price, gas_limit, to, value, input, access_list, y_parity, r, s])`

**Access list encoding**

`access_list` is an RLP list of access list items. Each access list item is:

`rlp([address, storage_keys])`

Where:
- `address` is an RLP string of length 20.
- `storage_keys` is an RLP list of 32-byte storage keys (each is an RLP string of length 32).

#### 2.1.5 EIP-1559 transaction (typed, type 0x02)

EIP-1559 transactions are EIP-2718 typed transactions with type byte `0x02`.

**RLP payload field order (unsigned fields)**

`[chain_id, nonce, max_priority_fee_per_gas, max_fee_per_gas, gas_limit, to, value, input, access_list]`

Where:
- `to` is a transaction kind:
  - Call: 20-byte address
  - Create: empty byte string

**Signature fields**

The signed transaction appends:
- `y_parity` (0 or 1)
- `r`
- `s`

**Full EIP-2718 encoding**

`0x02 || rlp([chain_id, nonce, max_priority_fee_per_gas, max_fee_per_gas, gas_limit, to, value, input, access_list, y_parity, r, s])`

#### 2.1.6 EIP-7702 transaction (typed, type 0x04)

EIP-7702 transactions are EIP-2718 typed transactions with type byte `0x04`.

**Key constraint**

In this implementation model, the EIP-7702 transaction’s `to` is always an address (no create transactions of this type).

**RLP payload field order (unsigned fields)**

`[chain_id, nonce, max_priority_fee_per_gas, max_fee_per_gas, gas_limit, to, value, input, access_list, authorization_list]`

**Authorization list encoding**

`authorization_list` is an RLP list of signed authorizations, each encoded as:

`rlp([auth_chain_id, auth_address, auth_nonce, y_parity, r, s])`

Where:
- `auth_chain_id` is a uint256 (RLP scalar). For validation, implementations SHOULD treat `auth_chain_id` equal to 0 as “wildcard”, otherwise it MUST match the transaction’s chain id.
- `auth_address` is a 20-byte address.
- `auth_nonce` is a u64.
- `y_parity`, `r`, `s` are ECDSA signature components for the authorization.

The authorization signature hash for an authorization item is:
- `keccak256( 0x05 || rlp([auth_chain_id, auth_address, auth_nonce]) )`

**Signature fields**

The signed transaction appends:
- `y_parity` (0 or 1)
- `r`
- `s`

**Full EIP-2718 encoding**

`0x04 || rlp([chain_id, nonce, max_priority_fee_per_gas, max_fee_per_gas, gas_limit, to, value, input, access_list, authorization_list, y_parity, r, s])`

### 2.2 Receipt Types

Receipts follow Ethereum’s receipt model with EIP-2718 typed envelopes for typed transactions, and with additional metadata for deposit transactions.

#### 2.2.1 Common receipt fields

All receipts include the standard Ethereum receipt components:
- `status`: EIP-658 status code (0 or 1)
- `cumulative_gas_used`: cumulative gas used in the block up to and including this transaction
- `logs_bloom`: 256-byte bloom filter (2048 bits)
- `logs`: list of log entries

These four fields are committed in the receipts trie root (§3.3.1) and are therefore consensus-critical.

**Receipt RLP payload encoding (inner receipt)**

The inner receipt payload is the RLP list:

`rlp([status, cumulative_gas_used, logs_bloom, logs])`

Encoding rules:
- `status` is encoded as an RLP scalar: `0x00` for failure, `0x01` for success.
- `cumulative_gas_used` is encoded as an RLP scalar (no leading zeros).
- `logs_bloom` is encoded as an RLP string of length 256 bytes.
- `logs` is encoded as an RLP list of log entries (see §2.2.3).

**Receipt envelope encoding**

Receipts are encoded as:
- Legacy transactions: `rlp([status, cumulative_gas_used, logs_bloom, logs])` (no type byte prefix).
- Typed transactions: `type_byte || rlp([status, cumulative_gas_used, logs_bloom, logs])`, where:
  - EIP-2930 receipt type byte is `0x01`
  - EIP-1559 receipt type byte is `0x02`
  - EIP-7702 receipt type byte is `0x04`

#### 2.2.2 Deposit receipt

Deposit receipts extend the common receipt fields with:
- `deposit_nonce`: u64, required
- `deposit_receipt_version`: u64, required and MUST equal `1`

**Deposit nonce semantics**

For a deposit transaction with sender address `from`, the `deposit_nonce` in its receipt is:
- The account nonce of `from` **as read from the pre-transaction state** (i.e., before applying this transaction’s state changes).

**Deposit receipt RLP payload encoding**

Deposit receipts are encoded as an RLP list with two additional trailing fields:

`rlp([status, cumulative_gas_used, logs_bloom, logs, deposit_nonce, deposit_receipt_version])`

Where:
- `deposit_nonce` is an RLP scalar encoding of a u64.
- `deposit_receipt_version` is an RLP scalar encoding of the integer `1`.

**Deposit receipt envelope type**

Deposit receipts MUST be encoded as an EIP-2718 typed receipt:
- `0x7E || rlp([status, cumulative_gas_used, logs_bloom, logs, deposit_nonce, deposit_receipt_version])`

#### 2.2.3 Log encoding (consensus-critical)

Each log entry is encoded as an RLP list:

`rlp([address, topics, data])`

Where:
- `address` is an RLP string of length 20 bytes.
- `topics` is an RLP list of 32-byte values (each topic is an RLP string of length 32).
- `data` is an RLP string of length `len(data)` bytes.

### 2.3 Block Structure

This rollup EL uses the standard Ethereum block model (header + body), with additional constraints on certain header/body fields.

#### 2.3.1 Block body

The block body consists of:
- `transactions`: a list of EIP-2718 encoded transactions (including the L1 Block Info transaction and any deposit transactions)
- `ommers` (a.k.a. uncles): MUST be empty
- `withdrawals`: MUST be present and MUST be an empty list

#### 2.3.2 Block header

The block header is the standard Ethereum header with these rollup-specific interpretations and constraints:

- **`nonce`**: MUST be zero (8 bytes all `0x00`).
- **`ommers_hash`**: MUST equal the empty ommers root hash:
  - `0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347`
  - This is `keccak256( rlp([]) )`.
- **`extra_data`**:
  - MUST be exactly 17 bytes.
  - Encodes EIP-1559 parameter overrides and a minimum base fee (see §3.1.5).
- **`withdrawals_root`**:
  - MUST be present.
  - MUST commit to the storage root of the withdrawal message passer contract (see §9.1).
- **`requests_hash`**:
  - MUST be present and MUST equal the empty requests hash:
    - `0xe3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`
    - This is `sha256("")`.
- **`excess_blob_gas`**:
  - MUST be present and MUST equal 0.
- **`blob_gas_used`**:
  - MUST be present.
  - MUST equal the computed block DA-footprint used, derived during execution (see §3.3.4 and §5.2.4).

All other header fields follow Ethereum semantics unless otherwise specified here.

#### 2.3.3 Header field reference (types and constraints)

This table summarizes header fields commonly required to implement this rollup EL.

| Field | Type | Size | Constraint / meaning in this rollup EL |
|---|---|---:|---|
| `parent_hash` | bytes32 | 32 | MUST equal the hash of the parent header |
| `ommers_hash` | bytes32 | 32 | MUST equal `0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347` |
| `beneficiary` | address | 20 | Receives priority fees (tips) |
| `state_root` | bytes32 | 32 | MUST match post-execution state root |
| `transactions_root` | bytes32 | 32 | MUST match transaction trie root |
| `receipts_root` | bytes32 | 32 | MUST match receipt trie root |
| `logs_bloom` | bytes256 | 256 | MUST match bloom over all receipt logs |
| `difficulty` | uint256 | 32 | Standard post-merge meaning; treated as provided |
| `number` | uint64 | 8 | MUST equal parent number + 1 |
| `gas_limit` | uint64 | 8 | Block gas limit; also used as DA-footprint limit (§5.2.4) |
| `gas_used` | uint64 | 8 | MUST match post-execution gas used |
| `timestamp` | uint64 | 8 | MUST be strictly greater than parent timestamp |
| `extra_data` | bytes | ≤ 32 | MUST be exactly 17 bytes (version 1 encoding, §3.1.5) |
| `mix_hash` | bytes32 | 32 | Used as `prev_randao` value (standard post-merge meaning) |
| `nonce` | bytes8 | 8 | MUST be zero (`0x0000000000000000`) |
| `base_fee_per_gas` | uint64 | 8 | MUST satisfy §3.1.6 |
| `withdrawals_root` | bytes32 | 32 | MUST equal storage root of message passer contract post-exec (§9.2) |
| `parent_beacon_block_root` | bytes32? | 32 | Optional; if present MUST match payload attributes |
| `excess_blob_gas` | uint64 | 8 | MUST be present and MUST equal 0 |
| `blob_gas_used` | uint64 | 8 | MUST be present and MUST equal DA-footprint used (§3.3.4, §5.2.4) |
| `requests_hash` | bytes32 | 32 | MUST be present and MUST equal `0xe3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855` |

### 2.4 RLP Encoding (normative)

This rollup EL uses Ethereum’s Recursive Length Prefix (RLP) encoding for:
- Legacy transactions (untyped)
- Transaction payloads for typed transactions
- Receipt payloads and receipt envelopes
- Merkle-Patricia trie node serialization (via Ethereum’s standard rules)

Implementations MUST follow the RLP rules below exactly.

#### 2.4.1 RLP byte strings

RLP encodes a byte string `S` (a sequence of 0 or more bytes) as follows:

Let `len = |S|` be the length in bytes.

1. **Single-byte, value < 0x80**
   - If `len == 1` and `S[0] < 0x80`, then the RLP encoding is exactly the single byte `S[0]`.

2. **Short string (0–55 bytes)**
   - If `0 <= len <= 55` and the single-byte rule above does not apply, then:
     - prefix = `0x80 + len`
     - encoding = `prefix || S`
   - The empty string has `len == 0` and is encoded as the single byte `0x80`.

3. **Long string (>= 56 bytes)**
   - If `len >= 56`, let `len_be` be the minimal big-endian byte string encoding of `len` (no leading zeros).
   - Let `len_of_len = |len_be|`.
   - Then:
     - prefix = `0xB7 + len_of_len`
     - encoding = `prefix || len_be || S`

#### 2.4.2 RLP lists

RLP encodes a list `L` (a sequence of RLP items) by first encoding each element and concatenating them to form a payload.

Let `payload = rlp(item_0) || rlp(item_1) || ... || rlp(item_n)`.
Let `len = |payload|`.

1. **Short list (0–55 bytes)**
   - If `0 <= len <= 55`:
     - prefix = `0xC0 + len`
     - encoding = `prefix || payload`
   - The empty list has `len == 0` and is encoded as the single byte `0xC0`.

2. **Long list (>= 56 bytes)**
   - If `len >= 56`, let `len_be` be the minimal big-endian byte string encoding of `len` (no leading zeros).
   - Let `len_of_len = |len_be|`.
   - Then:
     - prefix = `0xF7 + len_of_len`
     - encoding = `prefix || len_be || payload`

#### 2.4.3 RLP integer (scalar) encoding

Many protocol fields are integers that are RLP-encoded as byte strings (“scalars”).

Define `to_be(x)` as the minimal big-endian byte string for the non-negative integer `x`:
- `to_be(0)` is the empty byte string.
- For `x > 0`, `to_be(x)` is the big-endian representation with no leading zeros.

Then the RLP scalar encoding of an integer `x` is:
- `rlp(to_be(x))` using the byte-string rules in §2.4.1.

Notes:
- This implies `0` is encoded as `0x80` (the empty string in RLP), not as `0x00`.
- Integers MUST NOT be encoded with leading zeros.

#### 2.4.4 Common RLP examples (non-normative)

These examples are illustrative and not exhaustive:
- Empty string: `""` → `0x80`
- Empty list: `[]` → `0xC0`
- Single byte `0x00` (string length 1, < 0x80) → `0x00`
- Single byte `0x7F` → `0x7F`
- Single byte `0x80` (not < 0x80) → `0x8180`
- The list `[ "" ]` → `0xC180`

### 2.5 Transaction and Receipt Trie Roots (normative)

Block headers commit to transactions and receipts using Ethereum’s standard Merkle-Patricia trie root construction.

This specification assumes the standard Ethereum trie node encoding and hashing rules. Implementations MUST match Ethereum consensus behavior for these roots.

#### 2.5.1 Transaction trie root (`transactions_root`)

Let `txs[i]` be the canonical transaction encoding bytes for the transaction at index `i` in the block body.

The transaction trie is constructed as:
- **Key** for index `i`: the RLP encoding of the integer `i` (as an RLP scalar).
- **Value**: the transaction encoding bytes `txs[i]` (as a byte string inserted into the trie value).

The root hash is the `transactions_root` committed in the header.

#### 2.5.2 Receipt trie root (`receipts_root`)

Let `receipts[i]` be the canonical receipt envelope encoding bytes for the receipt at index `i`:
- For legacy receipts: `rlp([status, cumulative_gas_used, logs_bloom, logs])`
- For typed receipts: `type_byte || rlp([status, cumulative_gas_used, logs_bloom, logs])`
- For deposit receipts: `0x7E || rlp([status, cumulative_gas_used, logs_bloom, logs, deposit_nonce, deposit_receipt_version])`

The receipt trie is constructed as:
- **Key** for index `i`: the RLP encoding of the integer `i` (as an RLP scalar).
- **Value**: the receipt envelope bytes `receipts[i]`.

The root hash is the `receipts_root` committed in the header.

#### 2.5.3 Logs bloom (`logs_bloom`)

The block header `logs_bloom` is computed from all logs in all receipts in the block body, using the standard Ethereum bloom filter rules:
- Bloom is 2048 bits (256 bytes).
- Each log contributes bits derived from Keccak-256 hashes of:
  - The log address, and
  - Each log topic.

The resulting bloom MUST match the header’s `logs_bloom`.


## 3. Consensus Rules

This section specifies validity rules for blocks and transactions as processed by the rollup EL.

### 3.1 Header Validation

Header validation MUST be performed before block execution. A block with an invalid header MUST be rejected.

#### 3.1.1 Fixed-value header constraints

Let `H` be the block header being validated.

- **`H.nonce`** MUST equal `0x0000000000000000` (8 bytes).
- **`H.ommers_hash`** MUST equal:
  - `0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347`
  - (the Keccak-256 of the RLP encoding of an empty list).
- **`H.extra_data`** MUST have length ≤ 32 bytes (Ethereum constraint) and MUST additionally satisfy the rollup-specific encoding constraints in §3.1.5.
- **`H.gas_used`** MUST be ≤ `H.gas_limit`.
- **`H.gas_limit`** MUST satisfy the standard Ethereum gas-limit constraints relative to its parent (delta bounds). The rollup does not override this rule.

#### 3.1.2 Parent-relative constraints

Let `P` be the parent header and `H` the candidate header, with:
- `H.parent_hash == hash(P)`.

Then:
- `H.number == P.number + 1`.
- `H.timestamp` MUST be strictly greater than `P.timestamp`.

#### 3.1.3 DA-footprint header constraints (`blob_gas_used`, `excess_blob_gas`)

Under the current rule set, the header fields are interpreted as **DA-footprint accounting**:

- `H.excess_blob_gas` MUST be present and MUST equal `0`.
- `H.blob_gas_used` MUST be present. Its value is validated post-execution (see §3.3.4).

There are **no EIP-4844 blob transactions** in this rollup EL transaction set; these header fields exist solely for rollup DA-footprint accounting.

#### 3.1.4 Withdrawals-root and requests-hash header constraints

- `H.withdrawals_root` MUST be present and is validated post-execution as specified in §9.1.
- `H.requests_hash` MUST be present and MUST equal:
  - `0xe3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`

#### 3.1.5 Block header `extra_data` encoding (version 1)

The header field `extra_data` MUST be exactly 17 bytes and is encoded as:

| Offset | Size | Name | Type | Description |
|---:|---:|---|---|---|
| 0 | 1 | `version` | u8 | MUST equal `0x01` |
| 1 | 4 | `denominator` | u32 | EIP-1559 max change denominator (big-endian) |
| 5 | 4 | `elasticity` | u32 | EIP-1559 elasticity multiplier (big-endian) |
| 9 | 8 | `min_base_fee` | u64 | Minimum base fee per gas (big-endian) |

Interpretation rules:
- If `denominator == 0` AND `elasticity == 0`, then the chain’s configured default EIP-1559 parameters apply (implementation-provided chain configuration). Otherwise, the provided `denominator` and `elasticity` values are used for base fee computation (see §3.1.6).
- `min_base_fee` MUST be enforced as a lower bound on the computed next base fee (see §3.1.6).

#### 3.1.6 Base fee per gas computation

Let:
- `P` be the parent header.
- `H` be the candidate header.
- `base_fee_parent = P.base_fee_per_gas` (if missing, treat as 0).
- `gas_limit_parent = P.gas_limit`.
- `gas_used_parent = P.gas_used`.
- `da_used_parent = P.blob_gas_used` (if missing, treat as 0).

Let the EIP-1559 parameters be:
- `elasticity` and `denominator` decoded from `P.extra_data` (§3.1.5), unless `(elasticity, denominator) = (0, 0)` in which case chain defaults are used.

Define:
- `effective_gas_used_parent = max(gas_used_parent, da_used_parent)`.
- `target = floor(gas_limit_parent / elasticity)`.

Then the next block base fee **before** applying `min_base_fee` is computed by the standard EIP-1559 rule:

- If `effective_gas_used_parent == target`:
  - `base_fee_next_raw = base_fee_parent`
- Else if `effective_gas_used_parent > target`:
  - `delta = effective_gas_used_parent - target`
  - `base_fee_change = max( floor(base_fee_parent * delta / target / denominator), 1 )`
  - `base_fee_next_raw = base_fee_parent + base_fee_change`
- Else (`effective_gas_used_parent < target`):
  - `delta = target - effective_gas_used_parent`
  - `base_fee_change = floor(base_fee_parent * delta / target / denominator)`
  - `base_fee_next_raw = max(base_fee_parent - base_fee_change, 0)`

Finally, apply the minimum base fee from `P.extra_data`:
- `base_fee_next = max(base_fee_next_raw, min_base_fee)`

The candidate header MUST satisfy:
- `H.base_fee_per_gas == base_fee_next`

### 3.2 Block Validation (Pre-Execution)

Pre-execution validation checks the block’s internal consistency before running the EVM.

Let `B` be the block with header `H` and body `Body`.

#### 3.2.1 Ommers MUST be empty

- `Body.ommers` MUST be empty.
- `H.ommers_hash` MUST equal the empty ommers root hash (already checked in §3.1.1).

#### 3.2.2 Transaction root

Let `txs` be the ordered list of transactions in `Body.transactions`, each in their **canonical** EIP-2718/legacy encoding as included in the block.

The header MUST commit to the transactions list via the standard Ethereum transaction trie root rule:
- `H.transactions_root == transactionsTrieRoot(txs)`

Where `transactionsTrieRoot` is the Keccak-256 Merkle-Patricia trie root of the RLP-encoded transaction list items indexed by their transaction index (Ethereum consensus rule).

#### 3.2.3 Required first transaction: L1 Block Info transaction

For any block with `H.number != genesis_number`:
- `len(Body.transactions)` MUST be ≥ 1.
- `Body.transactions[0]` MUST be an **L1 Block Info transaction** as specified in §4.1–§4.2.

For the genesis block:
- No L1 Block Info transaction is required.

#### 3.2.4 Withdrawals list MUST be present and empty

- `Body.withdrawals` MUST be present (not omitted).
- `Body.withdrawals` MUST be an empty list.

#### 3.2.5 Presence of withdrawals_root and blob_gas_used

The header MUST include:
- `H.withdrawals_root` (validated post-execution; see §9.1).
- `H.blob_gas_used` (validated post-execution; see §3.3.4).

### 3.3 Block Validation (Post-Execution)

Post-execution validation checks that executing the block’s transactions against the parent state yields:
- The committed roots and header summary fields.
- The DA-footprint accounting commitments.

#### 3.3.1 Receipts root

Let `receipts` be the ordered list of receipts produced by executing the block’s transactions.

The header MUST satisfy:
- `H.receipts_root == receiptsTrieRoot(receipts)`

Where `receiptsTrieRoot` is the standard Ethereum receipt trie root:
- Each receipt is encoded in its canonical envelope form (legacy or typed) and inserted into the trie keyed by transaction index.

#### 3.3.2 Gas used

Let `gas_used_exec` be the total gas used reported by execution (Ethereum semantics).

The header MUST satisfy:
- `H.gas_used == gas_used_exec`

#### 3.3.3 State root

Let `state_root_exec` be the post-state root after executing all transactions.

The header MUST satisfy:
- `H.state_root == state_root_exec`

#### 3.3.4 DA-footprint used (`blob_gas_used`)

Let `da_used_exec` be the block’s computed DA-footprint used from execution (defined in §5.2.4).

The header MUST satisfy:
- `H.blob_gas_used == da_used_exec`

### 3.4 Execution order and additional validation (state transition)

This rollup EL executes blocks using standard Ethereum state transition rules, with additional rollup-specific validations and accounting.

#### 3.4.1 Pre-execution block changes

Before executing any block transactions, the EL applies standard Ethereum pre-block system updates (including system contract calls related to block hashes and beacon roots when applicable).

These pre-execution changes are consensus-critical and MUST match Ethereum semantics for the configured EVM ruleset.

#### 3.4.2 Per-transaction validation and execution ordering

Transactions are executed in block order, from index 0 to the end of the transaction list.

For each transaction:

1. **Determine transaction type**
   - If deposit (`type_byte == 0x7E`): apply deposit-specific validation and charging rules (§6).
   - Otherwise: apply standard Ethereum validation plus rollup-specific fee rules (§5.4).

2. **DA-footprint limit check (non-deposit only)**
   - Compute `tx_da_footprint` (§5.2.4).
   - Maintain a running sum `da_footprint_used`.
   - Reject the block if `da_footprint_used + tx_da_footprint > block_gas_limit`.

3. **Gas limit check**
   - Reject the block if `gas_used_so_far + tx.gas_limit > block_gas_limit` under standard Ethereum rules.

4. **Execute the EVM transaction**
   - Apply EVM state changes and produce an execution result (status, logs, gas used).

5. **Receipt construction**
   - Construct a receipt envelope matching the transaction type (§2.2).
   - For deposits, include `deposit_nonce` and `deposit_receipt_version` (§2.2.2).

#### 3.4.3 Post-block outputs

After executing all transactions, the EL produces:
- `state_root_exec` (post-state root)
- `receipts_root_exec` (receipt trie root)
- `gas_used_exec` (total gas used)
- `block_da_footprint_used` (DA-footprint sum, committed as `blob_gas_used`)
- `withdrawals_root_exec` (message passer storage root, committed as `withdrawals_root`)

These outputs are validated against the header as specified in §3.3 and §9.2.

## 4. L1 Block Info System

This section specifies how L1 origin information and fee parameters are injected into each L2 block and made available to the EVM.

### 4.1 L1 Block Info Transaction

For every non-genesis block, the first transaction in the block MUST be an **L1 Block Info transaction**.

#### 4.1.1 Placement

Let `txs` be the block’s transaction list. For any non-genesis block:
- `txs[0]` MUST be the L1 Block Info transaction.

#### 4.1.2 Transaction envelope

The L1 Block Info transaction MUST be encoded as a **deposit transaction** (type `0x7E`), because it is L1-authorized and does not require an ECDSA signature on L2.

The deposit transaction fields MUST satisfy:
- `to` MUST be the L1 Block Info contract address: `0x4200000000000000000000000000000000000015`
- `from` MUST be `0xDeaDDeaDDeaDDeaDDeaDDeaDDeaDDeaDDeaD0001`
- `mint` MUST be 0
- `value` MUST be 0
- `is_system_transaction` MUST be `0x00`
- `input` MUST be the calldata encoding specified in §4.2

The `source_hash` field is not interpreted by the EL for this transaction and is treated as an opaque bytes32 value.

### 4.2 L1 Block Info Format (latest calldata)

The L1 Block Info transaction `input` is an ABI-encoded calldata payload that begins with a 4-byte function selector followed by fixed-length fields.

#### 4.2.1 Function selector

The first 4 bytes of calldata MUST equal:
- `0x3db6be2b`

#### 4.2.2 Calldata length

Let `calldata` be the transaction `input` bytes.

- `len(calldata)` MUST equal `4 + 174 = 178` bytes.
- Let `data = calldata[4..]` be the 174-byte tail after the selector.
- Parsing MUST use fixed offsets as specified below.

#### 4.2.3 Calldata field layout (byte offsets)

All integers are big-endian. All `uint256` values are 32-byte big-endian unsigned integers.

Layout of `data` (i.e., offsets start at 0 immediately after the 4-byte selector):

| Offset | Size | Name | Type | Notes |
|---:|---:|---|---|---|
| 0 | 4 | `l1_base_fee_scalar` | uint32 | Used for L1 fee computation |
| 4 | 4 | `l1_blob_base_fee_scalar` | uint32 | Used for L1 fee computation |
| 8 | 8 | `sequence_number` | uint64 | Present but not required by EL fee rules |
| 16 | 8 | `l1_timestamp` | uint64 | Present but not required by EL fee rules |
| 24 | 8 | `l1_number` | uint64 | Present but not required by EL fee rules |
| 32 | 32 | `l1_base_fee` | uint256 | Used for L1 fee computation |
| 64 | 32 | `l1_blob_base_fee` | uint256 | Used for L1 fee computation |
| 96 | 32 | `l1_hash` | bytes32 | Present but not required by EL fee rules |
| 128 | 32 | `batcher_hash` | bytes32 | Present but not required by EL fee rules |
| 160 | 4 | `operator_fee_scalar` | uint32 | Used for operator fee computation |
| 164 | 8 | `operator_fee_constant` | uint64 | Used for operator fee computation |
| 172 | 2 | `da_footprint_gas_scalar` | uint16 | Used for DA-footprint accounting |

The EL treats these as the authoritative per-block parameters for the fee rules defined in §5.

### 4.3 L1 Block Info Contract

The L1 Block Info contract is a predeployed contract at:
- `L1_BLOCK_CONTRACT = 0x4200000000000000000000000000000000000015`

The EL reads fee parameters from this contract’s storage during transaction validation and execution.

#### 4.3.1 Storage slots used by the EL

The EL reads the following storage slots and packed offsets:

##### Slot 1: L1 base fee

- **Slot index**: `1`
- **Value**: `l1_base_fee` (uint256)

##### Slot 3: fee scalars packing

- **Slot index**: `3`
- The 32-byte big-endian slot value packs:
  - `l1_base_fee_scalar` as a big-endian uint32 at byte offsets `[16..20)`
  - `l1_blob_base_fee_scalar` as a big-endian uint32 at byte offsets `[20..24)`

##### Slot 7: L1 blob base fee

- **Slot index**: `7`
- **Value**: `l1_blob_base_fee` (uint256)

##### Slot 8: operator fee and DA-footprint packing

- **Slot index**: `8`
- The 32-byte big-endian slot value packs:
  - `da_footprint_gas_scalar` as a big-endian uint16 at byte offsets `[18..20)`
  - `operator_fee_scalar` as a big-endian uint32 at byte offsets `[20..24)`
  - `operator_fee_constant` as a big-endian uint64 at byte offsets `[24..32)`

#### 4.3.2 Fee recipients (vault addresses)

Fees are credited to the following predeployed recipient addresses:

- **Base fee recipient**: `0x4200000000000000000000000000000000000019`
- **L1 fee recipient**: `0x420000000000000000000000000000000000001A`
- **Operator fee recipient**: `0x420000000000000000000000000000000000001B`

The block beneficiary (header `coinbase` / `beneficiary`) receives the transaction priority fee according to Ethereum semantics.

### 4.4 L1 Block Info extraction and validity conditions

The rollup EL extracts L1 block info parameters from the first transaction in each non-genesis block.

#### 4.4.1 Extraction procedure

Given a block body `Body`:
- If `Body.transactions` is empty and the block is not the genesis block, the block MUST be rejected.
- Otherwise, let `tx0 = Body.transactions[0]`. The EL extracts the L1 block info parameters from `tx0.input`.

#### 4.4.2 Calldata validity

Let `calldata = tx0.input`:
- If `len(calldata) < 4`, the block MUST be rejected.
- Let `selector = calldata[0..4]`.
  - The selector MUST equal `0x3db6be2b`.
- Let `data = calldata[4..]`.
  - `len(data)` MUST equal `174`.

If any of these conditions fail, the block MUST be rejected as having an invalid L1 Block Info transaction.

#### 4.4.3 Genesis exception

The genesis block is the only block permitted to omit the L1 Block Info transaction.

For genesis:
- Fee parameters derived from the L1 Block Info contract storage are chain-config dependent and are not required to be updated by an L1 Block Info transaction.


## 5. Fee Calculation

This section specifies all fee components charged by the rollup EL under the current rule set.

### 5.1 L2 Execution Fee

For non-deposit transactions, the rollup EL charges standard Ethereum execution fees:

- Let `gas_used` be the transaction gas used.
- Let `effective_gas_price` be the transaction’s effective gas price computed per Ethereum rules for its transaction type (legacy / EIP-1559 / etc.).

Then the **priority fee** is credited to the block beneficiary, and the **base fee** is credited to the base fee recipient (§4.3.2), following Ethereum fee semantics.

The base fee per gas used for these calculations is the block’s `base_fee_per_gas`.

### 5.2 L1 Data Fee and DA-footprint accounting

Non-deposit transactions are charged an additional fee representing L1 data availability costs.

This subsystem includes:
- A per-transaction **L1 data fee** (credited to the L1 fee recipient).
- A per-transaction **data gas** metric (for RPC visibility).
- A per-transaction and per-block **DA-footprint** metric used to enforce a block-wide DA-footprint limit committed via `blob_gas_used`.

Deposit transactions are exempt from these costs.

#### 5.2.1 Applicability

Let `tx_bytes` be the EIP-2718 encoding of a transaction as included in the block.

- If `tx_bytes` is empty: treat fees as zero.
- If `tx_bytes[0] == 0x7E` (deposit transaction): L1 data fee and operator fee MUST be zero.
- Otherwise: apply the rules below.

#### 5.2.2 FastLZ-based estimated compressed size

Define the function:
- `fastlz_size = FastLZ_Compressed_Length(tx_bytes)` (an integer number of bytes)

Then define:

- `L1_COST_FASTLZ_COEF = 836_500`
- `L1_COST_INTERCEPT = 42_585_600`
- `MIN_TX_SIZE_SCALED = 100 * 1_000_000`

Compute:
- `size_scaled = max( fastlz_size * L1_COST_FASTLZ_COEF - L1_COST_INTERCEPT, MIN_TX_SIZE_SCALED )`

Where:
- All arithmetic is performed over non-negative integers.
- The result is in “bytes scaled by 1e6”.

Define:
- `estimated_size_bytes = floor(size_scaled / 1_000_000)`

The FastLZ compressed length function MUST match the specific FastLZ variant used by the reference implementation for consensus-critical DA-footprint computations.

#### 5.2.3 L1 data gas (for observability)

Let `size_scaled` be the scaled estimated size from §5.2.2.

Define:
- `NON_ZERO_BYTE_COST = 16`

Then:
- `l1_data_gas = floor( size_scaled * NON_ZERO_BYTE_COST / 1_000_000 )`

This corresponds to `estimated_size_bytes * 16` after integer division.

#### 5.2.4 DA-footprint per transaction and block (`blob_gas_used`)

Let:
- `estimated_size_bytes = floor(size_scaled / 1_000_000)` from §5.2.2.
- `da_footprint_gas_scalar` be the uint16 read from the L1 Block Info contract slot 8 packing (§4.3.1).

For a non-deposit transaction:
- `tx_da_footprint = estimated_size_bytes * da_footprint_gas_scalar`

For a deposit transaction:
- `tx_da_footprint = 0`

For a block, define:
- `block_da_footprint_used = sum(tx_da_footprint for all transactions in block)`

The rollup EL enforces a DA-footprint block limit by requiring:
- `block_da_footprint_used <= block_gas_limit`

The post-execution value committed in the block header MUST satisfy:
- `header.blob_gas_used == block_da_footprint_used` (see §3.3.4).

#### 5.2.5 L1 data fee (credited to L1 fee recipient)

Let:
- `l1_base_fee` (uint256) be read from L1 Block Info contract slot 1.
- `l1_blob_base_fee` (uint256) be read from L1 Block Info contract slot 7.
- `l1_base_fee_scalar` (uint32) be read from L1 Block Info contract slot 3 packing (§4.3.1).
- `l1_blob_base_fee_scalar` (uint32) be read from L1 Block Info contract slot 3 packing (§4.3.1).
- `size_scaled` be computed from §5.2.2 as an integer.

Compute:

- `l1_fee_scaled = (l1_base_fee * 16 * l1_base_fee_scalar) + (l1_blob_base_fee * l1_blob_base_fee_scalar)`
- `l1_data_fee = floor( size_scaled * l1_fee_scaled / 1_000_000_000_000 )`

`l1_data_fee` is denominated in wei and is credited to:
- `0x420000000000000000000000000000000000001A` (L1 fee recipient)

### 5.3 Operator Fee

Non-deposit transactions are charged an additional operator fee, credited to the operator fee recipient.

Let:
- `operator_fee_scalar` be the uint32 read from L1 Block Info contract slot 8 packing (§4.3.1).
- `operator_fee_constant` be the uint64 read from L1 Block Info contract slot 8 packing (§4.3.1).

Define:
- `OPERATOR_FEE_MULTIPLIER = 100`

The operator fee uses **gas-based charging** with a “limit-charge then refund” model:

#### 5.3.1 Up-front operator fee charge

For a non-deposit transaction with gas limit `gas_limit`:

- `operator_fee_charge = gas_limit * operator_fee_scalar * OPERATOR_FEE_MULTIPLIER + operator_fee_constant`

This amount is deducted from the sender balance prior to execution as part of the “additional cost” described in §5.4.

#### 5.3.2 Post-execution operator fee credit and refund

Let `gas_used` be the actual gas used by the transaction.

Compute:
- `operator_fee_cost = gas_used * operator_fee_scalar * OPERATOR_FEE_MULTIPLIER + operator_fee_constant`

Then:
- `operator_fee_cost` is credited to `0x420000000000000000000000000000000000001B` (operator fee recipient).

The difference `operator_fee_charge - operator_fee_cost` MUST be refunded to the sender, using the same effective mechanism as gas refunds (implementation-defined refund plumbing, consensus-critical net balance effect).

Deposit transactions MUST have operator fee equal to 0.

### 5.4 Total Fee Charged to Sender

For a non-deposit transaction:

1. Compute additional cost:
   - `additional_cost = l1_data_fee + operator_fee_charge`
2. Deduct `additional_cost` from the sender balance prior to normal Ethereum fee deduction.
3. Apply standard Ethereum execution fee logic (gas fee deduction, priority fee to beneficiary, base fee to base fee recipient).
4. Credit:
   - `l1_data_fee` to L1 fee recipient
   - `base_fee_per_gas * gas_used` to base fee recipient
   - `operator_fee_cost` to operator fee recipient
5. Refund operator fee difference as in §5.3.2.

For a deposit transaction:
- `l1_data_fee = 0`
- `operator_fee = 0`
- No L2 gas fee is charged to the sender; gas is still accounted for block `gas_used` and receipts.

### 5.5 Integer arithmetic conventions

All fee and DA-footprint computations in this specification use integer arithmetic with the following conventions:

- **Big-endian decoding**: fixed-width integers extracted from calldata and packed storage are interpreted as unsigned big-endian integers.
- **Rounding**: all divisions are integer divisions that round toward zero (i.e., `floor(a / b)` for non-negative integers).
- **Non-negativity**: all fee components are defined over non-negative integers.
- **Saturating behavior**: when an implementation uses bounded integer types internally, it MUST produce the same final mathematical results as if computed over unbounded non-negative integers, or explicitly saturate where the consensus rule saturates. In particular:
  - `max(...)` and `min(...)` must be applied after computing the relevant intermediate values.
  - `estimated_size_bytes = floor(size_scaled / 1_000_000)` MUST use integer division.

### 5.6 Fee crediting and recipients (consensus-critical balance effects)

For a non-deposit transaction, the following balance deltas MUST occur as part of the transaction’s execution:

- Let `beneficiary` be the block header `beneficiary` address.
- Let `base_fee_recipient = 0x4200000000000000000000000000000000000019`.
- Let `l1_fee_recipient = 0x420000000000000000000000000000000000001A`.
- Let `operator_fee_recipient = 0x420000000000000000000000000000000000001B`.

Let:
- `gas_used` be the transaction’s actual gas used.
- `base_fee` be the block `base_fee_per_gas`.
- `effective_gas_price` be the transaction’s effective gas price.

Define the **priority fee per gas**:
- `priority_fee_per_gas = max(effective_gas_price - base_fee, 0)`

Then:
- `balance[beneficiary] += priority_fee_per_gas * gas_used`
- `balance[base_fee_recipient] += base_fee * gas_used`
- `balance[l1_fee_recipient] += l1_data_fee`
- `balance[operator_fee_recipient] += operator_fee_cost`

Deposit transactions MUST NOT credit any of these fee recipients as part of fee charging (they may still transfer `value` and/or mint via normal state transitions).

### 5.7 Worked examples (illustrative)

These examples illustrate the fee formulas and integer rounding. They are not normative test vectors.

#### 5.7.1 Example: DA-footprint from scaled size

Assume:
- `size_scaled = 123_456_789` (bytes * 1e6)
- `da_footprint_gas_scalar = 10`

Then:
- `estimated_size_bytes = floor(123_456_789 / 1_000_000) = 123`
- `tx_da_footprint = 123 * 10 = 1_230`

#### 5.7.2 Example: data gas

Assume:
- `size_scaled = 123_456_789`

Then:
- `l1_data_gas = floor(123_456_789 * 16 / 1_000_000) = floor(1_975_308_624 / 1_000_000) = 1_975`

#### 5.7.3 Example: L1 data fee

Assume:
- `size_scaled = 200_000_000` (i.e., 200 bytes scaled by 1e6)
- `l1_base_fee = 1_000_000_000` wei
- `l1_base_fee_scalar = 2`
- `l1_blob_base_fee = 3_000_000_000` wei
- `l1_blob_base_fee_scalar = 4`

Compute:
- `l1_fee_scaled = (1_000_000_000 * 16 * 2) + (3_000_000_000 * 4)`
  - `= 32_000_000_000 + 12_000_000_000`
  - `= 44_000_000_000`
- `l1_data_fee = floor(200_000_000 * 44_000_000_000 / 1_000_000_000_000)`
  - `= floor(8_800_000_000_000_000_000 / 1_000_000_000_000)`
  - `= 8_800_000` wei

### 5.8 Effective gas price (per transaction type)

This subsection defines how `effective_gas_price` is determined for the supported transaction types.

Let `base_fee` be the block header `base_fee_per_gas` (u64).

#### 5.8.1 Legacy and EIP-2930

For legacy and EIP-2930 transactions:
- `effective_gas_price = gas_price`

#### 5.8.2 EIP-1559 and EIP-7702

For EIP-1559 and EIP-7702 transactions:
- Let `max_fee_per_gas` and `max_priority_fee_per_gas` be the transaction fields.
- The transaction is invalid if `max_fee_per_gas < base_fee`.
- Otherwise:
  - `effective_gas_price = min( max_fee_per_gas, base_fee + max_priority_fee_per_gas )`

#### 5.8.3 Deposit transactions

For deposit transactions:
- `effective_gas_price` MUST be treated as 0 for fee-charging purposes (no L2 gas fee deduction from the sender).


## 6. Deposit Transactions

This section specifies deposit transaction format (already defined in §2.1.2) and execution semantics.

### 6.1 Deposit Transaction Format

Deposit transaction type byte:
- `0x7E`

Field order and RLP encoding:
- As specified in §2.1.2.

### 6.2 Deposit Processing

Deposit transactions differ from regular transactions as follows:

#### 6.2.1 Signature and sender

- The sender is `from` from the transaction payload.
- No ECDSA signature checks are performed for deposit transactions.

#### 6.2.2 Fee rules

- Deposit transactions MUST NOT be charged L1 data fees.
- Deposit transactions MUST NOT be charged operator fees.
- Deposit transactions MUST NOT be charged L2 execution fees to the sender (no deduction based on gas used), even though `gas_used` is computed for accounting and receipts.

#### 6.2.3 Nonce handling

Deposit transactions increment the sender account nonce exactly once, following standard Ethereum transaction semantics:
- The sender nonce MUST be incremented by 1 for each deposit transaction, regardless of success or failure.

#### 6.2.4 Failure and halting semantics

Deposit transactions have special failure semantics:
- The `mint` amount MUST be persisted even if the deposit transaction fails or halts.
- The sender nonce MUST be incremented even if the deposit transaction fails or halts.
- The receipt status MUST reflect success (1) or failure (0) using EIP-658 semantics.
- Gas reporting in receipts follows the current rule set (deposit gas used is the actual gas used, with refunds enabled, as with regular transactions).

### 6.3 Mint Behavior

For a deposit transaction with:
- sender address `from`
- `mint` value `mint_amount` (u128 logical value)

The EL MUST apply the mint before applying any other balance deductions for that transaction:

- `balance[from] := balance[from] + mint_amount`

Then the EVM execution proceeds, applying `value` transfers and state changes.

If a deposit transaction fails:
- State changes other than the mint and nonce increment MUST be reverted.
- The mint and nonce increment MUST remain applied.

## 7. Transaction Pool

This section describes validity constraints and ordering rules for the local transaction pool (mempool). These rules affect which transactions a node will propose in blocks, but do not override consensus rules in §3.

### 7.1 Validation Rules

A transaction submitted to the pool MUST be rejected if any of the following holds:

- **Deposit transactions**: Transactions of type `0x7E` MUST NOT be accepted into the pool.
- **Blob transactions**: EIP-4844 blob transactions MUST NOT be accepted into the pool.
- **Missing envelope bytes**: For non-deposit transactions, the pool MUST retain the canonical EIP-2718 encoded bytes; transactions that cannot supply these bytes MUST be rejected.
- **Standard Ethereum checks**: All standard signature, nonce, intrinsic gas, and fee-cap checks for the transaction type MUST pass against the node’s current state.
- **Additional-cost affordability**: The sender MUST have sufficient balance to cover the up-front additional cost (`l1_data_fee + operator_fee_charge`) in addition to standard Ethereum max-fee requirements (see §5.4).

### 7.2 Ordering

The pool provides transactions for block building in an order that prioritizes block profitability under the standard Ethereum “tip-based” model:
- Transactions are selected to maximize effective priority fee to the block beneficiary while respecting per-sender nonce order.

The rollup EL block builder MAY additionally filter or skip transactions that do not fit within:
- The block gas limit, and
- The block DA-footprint limit (`block_da_footprint_used <= block_gas_limit`, §5.2.4).

### 7.3 Pool-specific metadata

The rollup node’s transaction pool may track additional metadata on top of the consensus transaction encoding. This metadata is **not** part of the transaction hash and is not committed on-chain, but it can affect which transactions a node will consider for block inclusion.

#### 7.3.1 Conditional transactions

A pooled transaction MAY carry an optional “conditional” constraint.

If present, a conditional constraint SHOULD be evaluated by the node when selecting transactions for inclusion. If the condition does not hold at selection time, the transaction SHOULD be skipped for inclusion.

This mechanism does not change consensus validity; a block MAY include a transaction regardless of whether a local node would have considered it “conditional”.

#### 7.3.2 Interop deadline

A pooled transaction MAY carry an optional “interop deadline” timestamp (u64) that represents a local validity window for the node’s transaction selection logic.

If present:
- A node SHOULD treat the transaction as eligible only up to the given deadline.
- After the deadline passes, the node SHOULD drop or deprioritize the transaction.

This mechanism does not change consensus validity; a block MAY include a transaction even if it was past a local node’s interop deadline.

## 8. Block Building (Payload Construction)

This section specifies the rules for constructing a block payload as accepted by the rollup execution engine.

### 8.1 Payload Attributes

Payload building is parameterized by a set of attributes:

- `timestamp` (u64): the block timestamp.
- `suggested_fee_recipient` (address): the block beneficiary.
- `prev_randao` (bytes32): randomness value.
- `parent_beacon_block_root` (optional bytes32): parent beacon root if provided.
- `withdrawals` (list): MUST be provided as an empty list for this rollup EL.

Rollup-specific attributes:
- `no_tx_pool` (bool): if true, do not include pool transactions.
- `transactions` (list of bytes): ordered list of transactions provided by the sequencer; each item is the raw EIP-2718/legacy transaction encoding.
- `gas_limit` (optional u64): override block gas limit.
- `eip_1559_params` (8 bytes): packed parameters used to build header `extra_data`.
  - Let `p = eip_1559_params` be 8 bytes.
  - If `p == 0x0000000000000000`, then `denominator` and `elasticity` MUST be taken from the chain’s default base fee parameters.
  - Otherwise:
    - `denominator = be_u32(p[0..4])`
    - `elasticity = be_u32(p[4..8])`
- `min_base_fee` (u64): minimum base fee included in `extra_data` (required).

### 8.2 Block Assembly

Given a parent header `P` and payload attributes:

1. **Determine header extra data**:
   - Header `extra_data` MUST be encoded as version 1 (§3.1.5) using the provided `eip_1559_params` and `min_base_fee`.
2. **Initialize the block header fields** per Ethereum consensus, with rollup constraints:
   - `nonce = 0`
   - `ommers_hash = empty`
   - `withdrawals_root` present (validated post-execution)
   - `requests_hash = 0xe3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`
   - `excess_blob_gas = 0`
   - `blob_gas_used` present (validated post-execution)
3. **Apply pre-execution system calls**:
   - The EL performs mandatory pre-execution system contract updates (blockhashes and beacon root system calls) according to Ethereum semantics.
4. **Include sequencer-provided transactions**:
   - The block builder MUST execute the provided `transactions` in the exact order given.
   - The first transaction MUST be the L1 Block Info transaction (§4.1).
5. **Optionally include pool transactions**:
   - If `no_tx_pool` is false, the builder selects additional transactions from the pool subject to:
     - Block gas limit remaining.
     - Block DA-footprint remaining (§5.2.4).
   - Deposit and blob transactions MUST NOT be selected from the pool.

### 8.3 Gas Limits

The block gas limit is:
- The provided `gas_limit` if present, otherwise the parent-derived default (implementation-defined within Ethereum constraints).

The DA-footprint limit uses the same numeric value:
- `block_da_limit = block_gas_limit`

### 8.4 Header field derivation during block assembly

When building a block, the builder computes several header fields from executed outputs and fixed constants.

#### 8.4.1 Computed roots and bloom

After executing all transactions, the builder MUST compute:
- `transactions_root` from the canonical transaction encodings (§2.5.1).
- `receipts_root` from the canonical receipt encodings (§2.5.2).
- `logs_bloom` from the receipt logs (§2.5.3).
- `state_root` from the post-state.

These values MUST be committed into the header.

#### 8.4.2 DA-footprint commitment fields

The builder MUST set:
- `excess_blob_gas = 0`
- `blob_gas_used = block_da_footprint_used` (§5.2.4)

#### 8.4.3 Requests hash

The builder MUST set:
- `requests_hash = 0xe3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`

#### 8.4.4 Withdrawals root commitment

The builder MUST set:
- `withdrawals_root = storage_root(message_passer_contract)` after executing the block (§9.2)

And MUST include an empty withdrawals list in the body (§3.2.4).

## 9. Withdrawals

### 9.1 Withdrawal Mechanism

Withdrawals are represented by messages recorded in the predeployed L2-to-L1 message passer contract at:
- `0x4200000000000000000000000000000000000016`

### 9.2 Withdrawals storage root validation

For each block, the header field `withdrawals_root` is repurposed and MUST equal:
- The storage root of the message passer contract (`0x4200000000000000000000000000000000000016`) after executing the block.

Additionally:
- The block body withdrawals list MUST be present and MUST be empty (§3.2.4).

### 9.3 Storage root definition (for `withdrawals_root`)

This subsection defines the meaning of “storage root” as used by `withdrawals_root`.

#### 9.3.1 Ethereum account state components

In Ethereum’s state model, each account has:
- `nonce`
- `balance`
- `storage_root`
- `code_hash`

The `storage_root` is a commitment to the account’s storage as a Merkle-Patricia trie root.

#### 9.3.2 Storage trie key and value encoding

For a contract account, storage is a mapping from 256-bit storage slot keys to 256-bit values.

Let:
- `k` be a storage slot key (a 32-byte value).
- `v` be the stored 256-bit value at key `k`.

Then:
- The trie key is `keccak256(k)` interpreted as the 32-byte trie key.
- The trie value is the RLP encoding of `v` as an integer scalar:
  - `rlp(to_be(v))` where `to_be` is minimal big-endian without leading zeros (§2.4.3).

#### 9.3.3 Storage root computation

The account’s `storage_root` is the root hash of the Merkle-Patricia trie containing all `(keccak256(k), rlp(v))` pairs for all storage keys `k` that have been written to a non-zero value, following Ethereum’s standard storage trie rules.

#### 9.3.4 Withdrawal commitment rule

Let `S_pass` be the post-state of the block after executing all transactions.
Let `storage_root_pass` be the `storage_root` field of the account at:
- `0x4200000000000000000000000000000000000016`

Then:
- `header.withdrawals_root` MUST equal `storage_root_pass`.

## 10. RPC Extensions

This section specifies observable RPC differences and additions relevant to rollup behavior.

### 10.1 Modified Methods

#### 10.1.1 Transaction receipt fields

Transaction receipts returned by RPC include additional fields (where applicable):

- `l1Fee` (u128): the per-transaction L1 data fee in wei (§5.2.5). Omitted for deposits.
- `l1GasUsed` (u128): the per-transaction L1 data gas metric (§5.2.3). Omitted for deposits.
- `l1GasPrice` (u128): the L1 base fee used for fee computation (§4.2.3 / §4.3.1).
- `l1FeeScalar` (string-encoded float): MUST be omitted (null) under the current rule set.
- `l1BaseFeeScalar` (u128): L1 base fee scalar (§4.2.3 / §4.3.1).
- `l1BlobBaseFee` (u128): L1 blob base fee (§4.2.3 / §4.3.1).
- `l1BlobBaseFeeScalar` (u128): L1 blob base fee scalar (§4.2.3 / §4.3.1).
- `operatorFeeScalar` (u128): operator fee scalar (§4.2.3 / §4.3.1).
- `operatorFeeConstant` (u128): operator fee constant (§4.2.3 / §4.3.1).
- `daFootprintGasScalar` (u16): DA-footprint gas scalar (§4.2.3 / §4.3.1).

For deposit transactions:
- `depositNonce` (u64) MUST be present.
- `depositReceiptVersion` (u64) MUST be present and MUST equal `1`.

#### 10.1.2 Sending transactions

Nodes MAY forward user-submitted raw transactions to a sequencer endpoint for inclusion, while also retaining them locally for RPC visibility. This forwarding does not change consensus rules.

### 10.2 New Methods

The execution engine API uses rollup-specific payload envelopes and attributes that include:
- Ordered `transactions` bytes supplied by the sequencer.
- `no_tx_pool` flag.
- Additional parameters for header `extra_data` encoding (`eip_1559_params`, `min_base_fee`).

In addition to standard engine API expectations, rollup payload building and propagation typically includes:

#### 10.2.1 Payload attributes extensions

When requesting a new payload build, the caller provides:
- The standard payload attributes (`timestamp`, `suggested_fee_recipient`, `prev_randao`, `withdrawals`, optional `parent_beacon_block_root`).
- Rollup-specific fields:
  - `transactions`: an ordered list of raw transaction bytes that MUST be executed in-order by the block builder.
  - `no_tx_pool`: whether the builder may include additional transactions from the local pool.
  - `gas_limit`: an optional override for the block gas limit.
  - `eip_1559_params`: 8 bytes used to derive header `extra_data` (§8.1).
  - `min_base_fee`: u64 used to derive header `extra_data` (§3.1.5).

#### 10.2.2 Payload envelope extensions

When returning a built payload, a rollup engine MAY include additional derived fields to support verification and light integration:
- The computed `l2WithdrawalsRoot` (bytes32), which MUST equal the block header `withdrawals_root` (§9.2).
- The DA-footprint values as committed in the header (`blob_gas_used`, `excess_blob_gas`).

These fields are redundantly derivable from the block header and post-state; their inclusion is for API convenience and does not alter consensus rules.

## 11. Chain Configuration

### 11.1 Genesis Format

The rollup chain configuration includes:
- Standard Ethereum genesis fields (alloc, stateRoot, gasLimit, timestamp, etc.).
- A chain ID.
- Default EIP-1559 parameters used when header `extra_data` encodes zeros for `denominator` and `elasticity` (§3.1.5).

Hardfork-activation schedules and upgrade naming are intentionally omitted from this specification; a newly deployed chain MUST activate the behaviors described in this document from genesis via its chain specification mechanism.

### 11.2 System Contracts

Predeployed contract addresses referenced by this specification:

- **L1 Block Info contract**: `0x4200000000000000000000000000000000000015`
- **L2-to-L1 message passer**: `0x4200000000000000000000000000000000000016`
- **Base fee recipient**: `0x4200000000000000000000000000000000000019`
- **L1 fee recipient**: `0x420000000000000000000000000000000000001A`
- **Operator fee recipient**: `0x420000000000000000000000000000000000001B`

### 11.3 Fixed constants

The following constants are consensus-relevant within the scope of this rollup EL:

#### 11.3.1 Transaction and receipt type identifiers

- Deposit transaction type: `0x7E`
- Deposit receipt type: `0x7E`
- EIP-2930 transaction/receipt type: `0x01`
- EIP-1559 transaction/receipt type: `0x02`
- EIP-7702 transaction/receipt type: `0x04`

#### 11.3.2 Header constants

- Empty ommers root hash: `0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347`
- Empty requests hash: `0xe3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`

#### 11.3.3 L1 Block Info transaction constants

- Required selector: `0x3db6be2b`
- Required `to`: `0x4200000000000000000000000000000000000015`
- Required `from`: `0xDeaDDeaDDeaDDeaDDeaDDeaDDeaDDeaDDeaD0001`





