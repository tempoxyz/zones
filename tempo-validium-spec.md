# Tempo Validium Execution Layer Specification

This document specifies the behavior of a **Tempo L2 validium execution layer (EL)**. It intentionally omits historical upgrades and legacy formats. Normative keywords: **MUST**, **MUST NOT**, **SHOULD**, **SHOULD NOT**, **MAY**.

This spec focuses on:
- Block and transaction validity rules (consensus-layer validation for EL objects).
- State transition behavior relevant to validium-specific transaction types and fees.
- Byte-level encoding of serialized data (transactions, receipts, header extra data).
- Proof and settlement interfaces for ZK validity proofs.
- Privacy and data-availability assumptions.

## 1. Overview

### 1.1 What is the Tempo validium execution layer?

The Tempo validium EL is an Ethereum Virtual Machine (EVM) execution environment that:
- Executes EVM transactions and produces EVM state roots.
- Uses a **private sequencer** and **off-chain data availability (DA)**.
- Accepts **deposit** transactions originating from L1 and supports **withdrawals** from L2 to L1 via a message-passing contract.
- Commits execution results to L1 using **ZK validity proofs**.

### 1.2 Chain properties

The Tempo validium EL has the following protocol-level properties:
- **Tempo client** is used. There is **no native token**, so **no native-token bridging logic** exists.
- **No on-chain data availability**: transaction data are **not** posted to L1, and **no L1 data fee** exists.
- **Privacy**: block contents are visible only to the sequencer (and explicitly authorized parties).
- **ZK validity proofs** are used for settlement.
- **Fast finality assumption**: L1 origin blocks are treated as final before their deposits are included on L2.

### 1.3 Key concepts

#### 1.3.1 Sequencer

The sequencer is the actor that proposes L2 blocks and provides:
- The ordered list of L2 transactions for the block.
- The first “L1 Block Info” transaction which encodes L1 origin metadata and fee parameters used by the validium.

This document specifies **how blocks are validated and executed**, not sequencer selection or leader election.

#### 1.3.2 L1 origin metadata

Each L2 block is associated with an L1 “origin” block. A subset of the L1 origin block fields and rollup-specific parameters are made available on L2 by writing them into the **L1 Block Info contract** via the first transaction in each block.

#### 1.3.3 Deposits

Deposits are transactions forced onto L2 from L1. Deposits can:
- Call or create contracts.
- Carry calldata (`input` field).

Deposits are encoded as an EIP-2718 typed transaction with type byte `0x7E`.

#### 1.3.4 Withdrawals

Withdrawals are represented as L2 messages written into a predeployed message passer contract. The L2 block header commits to the **message passer contract’s storage root** using the header `withdrawals_root` field.

#### 1.3.5 Validity proofs

Each finalized L2 block (or batch of blocks) MUST be accompanied by a ZK validity proof attesting to:
- Correct EVM execution for all transactions in the block(s).
- Correct application of any system transactions (including L1 Block Info and deposit txs).
- Correct derivation of the resulting state root.

Proof formats and verifier contracts are **out of scope** for this document, but their interfaces are specified.

#### 1.3.6 Privacy and data availability

Transaction data are held off-chain by the sequencer. The protocol assumes:
- The sequencer withholds L2 transaction data from the public.
- Parties MAY receive data only if the sequencer grants access.
- L1 does not contain L2 transaction data, and cannot be used to reconstruct L2 state independently.

#### 1.3.7 Deposit authorization

Deposits are authorized by finalized L1 events from the L1 deposit contract. The sequencer MUST:
1. Watch finalized L1 blocks for deposit events.
2. Construct L2 deposit transactions from those events.
3. Include deposits only from L1 origin blocks treated as final.

The `source_hash` MUST uniquely bind to the L1 deposit event (at minimum the L1 block hash, log index, and event data). The EL does not validate `source_hash` derivation. Any L1 settlement contract MAY verify deposit inclusion using L1 receipt proofs; this is out of scope for this document.

## 2. Type Definitions

This section defines the **serialized formats** that are consensus-critical for this validium EL.

### 2.1 Transaction Types

The execution layer supports the following transaction envelopes:
- Legacy (RLP, no type byte)
- EIP-2930 (typed transaction)
- EIP-1559 (typed transaction, fixed base fee)
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
- `mint` MUST be zero. There is no native token.
- `value` MUST be zero. Any nonzero value is invalid.

### 2.2 Block Header Fields

The block header uses the same fields as Ethereum L1 with the following constraints:
- `withdrawals_root` is repurposed to commit to the **storage root of the withdrawal message passer contract**.
- `blob_gas_used` MUST be `0x0`.
- `excess_blob_gas` MUST be `0x0`.
- `base_fee_per_gas` MUST equal the fixed protocol constant `BASE_FEE_PER_GAS`.

### 2.3 L1 Block Info transaction

Every non-genesis L2 block MUST include an **L1 Block Info** system transaction as its first transaction.

Fields related to L1 data costs MUST be set to zero and are ignored.

## 3. Fees and Gas

### 3.0 Gas payment token

Because there is no native token, gas fees are paid in a designated **L2 gas token** (a TIP-20 or equivalent predeploy). Implementations MUST:
- Define a single gas payment token for the chain.
- Charge gas using the same accounting rules as Ethereum, denominated in that token.
- Reject transactions that cannot pay for gas in the gas token.

### 3.1 Fixed base fee

`base_fee_per_gas` is a fixed protocol constant (`BASE_FEE_PER_GAS`). It does not change block-to-block.

### 3.2 Priority fee (fixed base fee)

For legacy and EIP-2930 transactions, the `gas_price` field is used. The effective priority fee per gas is:

`priority_fee_per_gas = gas_price - BASE_FEE_PER_GAS`

Validity rules:
- `gas_price` MUST be greater than or equal to `BASE_FEE_PER_GAS`.
- `priority_fee_per_gas` MUST be nonnegative.

For EIP-1559 transactions, the effective gas price is:

`effective_gas_price = min(max_fee_per_gas, BASE_FEE_PER_GAS + max_priority_fee_per_gas)`

Validity rules:
- `max_fee_per_gas` MUST be greater than or equal to `BASE_FEE_PER_GAS`.
- `max_priority_fee_per_gas` MAY be zero.

The total fee paid is `gas_used * effective_gas_price`, denominated in the L2 gas token.

### 3.3 L1 data fees

There is **no L1 data availability fee**. Any fields or calculations related to L1 data cost MUST be ignored and treated as zero.

### 3.4 Operator fees

Operator fee logic is unchanged from the OP Stack EL unless explicitly redefined in an implementation-specific document. If operator fees are implemented, they MUST be denominated in the same token used for standard gas payment.

## 4. Privacy and Data Availability

### 4.1 Private block contents

Block bodies (transactions, receipts) are not public. The sequencer and authorized parties MAY access block contents via access-controlled channels.

### 4.2 DA responsibility

The protocol assumes that the sequencer:
- Retains the full transaction data.
- Can serve data to authorized users at its discretion.
- Can re-prove blocks from retained data.

Failure to serve data is considered an **availability failure** but does not invalidate blocks.

## 5. Proof and Settlement

### 5.1 Proof requirements

For each block or batch, a validity proof MUST attest that:
1. The L1 Block Info transaction is first and correctly updates the L1 Block Info contract.
2. All deposit transactions included are correctly formed and applied.
3. All user transactions are executed according to EVM rules.
4. The resulting state root matches the block header state root.
5. The message passer storage root matches `withdrawals_root`.

### 5.2 Settlement contract interface (abstract)

The L1 settlement contract MUST expose:
- `submitBatch(header, proof, public_inputs)` to post finalized headers and proofs.
- `finalizeWithdrawal(message, proof)` to verify L2 withdrawal messages.

Exact calldata formats and proof system details are **out of scope**.

## 6. Finality and Reorgs

### 6.1 L1 origin finality assumption

The validium EL assumes that the L1 origin block for any L2 block is **final** before that L2 block is produced. Therefore:
- L1 origin reorg handling is **not implemented**.
- Deposits from L1 are only included once their origin blocks are final.

This assumption requires a strong L1 finality signal (e.g., BFT finality or a conservative confirmation policy).

### 6.2 L2 reorg handling

The protocol does not provide an L2 reorg mechanism. The sequencer produces a single canonical chain. Any conflicting block is invalid, even if unproven.

## 7. State Transition Rules

The state transition is identical to Ethereum L1 with the following modifications:
- The first transaction in every non-genesis block MUST be the L1 Block Info system transaction.
- Deposit transactions are valid without signature checks.
- L1 data fees are not charged.
- `withdrawals_root` commits to the message passer storage root.

## 8. Tempo Node Changes

This section enumerates the changes required to a Tempo node to support this L2 validium.

### 8.1 New transaction type: Deposit (0x7E)

Tempo today supports transaction types `0x76` (TempoTransaction) and standard Ethereum types. The L2 adds:

- **Deposit transaction type `0x7E`** with fields: `source_hash`, `from`, `to`, `mint`, `value`, `gas_limit`, `is_system_transaction`, `input`.
- **Signature handling**: Deposit transactions have no ECDSA signature. The `from` field is the sender.
- **Validity rules**: `mint == 0`, `value == 0`, `is_system_transaction == 0`.

### 8.2 L1 Block Info system transaction

Tempo blocks today have no required first transaction. The L2 adds:

- **Every non-genesis block MUST begin with an L1 Block Info system transaction.**
- This transaction calls a predeploy contract to store L1 origin metadata (block number, block hash, timestamp).
- L2 contracts can read this metadata to reference L1 state.

### 8.3 Block header: `withdrawals_root`

Tempo today does not use `withdrawals_root`. The L2 adds:

- **`withdrawals_root` MUST contain the storage root of the message passer contract** after block execution.
- This commitment enables withdrawal proofs against finalized L2 state.

### 8.4 Message passer contract (withdrawal initiation)

Tempo today has no withdrawal mechanism. The L2 adds:

- **A predeploy message passer contract** that L2 users call to initiate withdrawals.
- The contract stores withdrawal messages in its storage.
- The storage root is committed to `withdrawals_root` in every block header.

### 8.5 No L2 reorgs

Tempo today may allow short forks before finalization. The L2 changes this:

- **The sequencer produces a single canonical chain.**
- **Any conflicting block is invalid**, even if unproven.
- There is no mechanism to reorg L2 blocks.

### 8.6 Deposit ingestion (sequencer behavior)

Tempo today does not ingest L1 deposits. The L2 adds:

- The sequencer watches finalized L1 deposit events.
- For each deposit event, the sequencer constructs an L2 deposit transaction with a deterministic `source_hash`.
- Deposits are only included from L1 blocks treated as final.

## 9. Invariants

- Every non-genesis block starts with a valid L1 Block Info transaction.
- `blob_gas_used == 0` and `excess_blob_gas == 0` for all blocks.
- `base_fee_per_gas` equals `BASE_FEE_PER_GAS` for all blocks.
- The block header `withdrawals_root` matches the message passer storage root after execution.
- No L1 data fee is ever charged or accounted for.
- A proven block is never reorged.
