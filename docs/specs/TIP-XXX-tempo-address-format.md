# TIP-XXX: Tempo Address Format

## Abstract

This TIP defines a human-readable address format for the Tempo ecosystem that:
- Is easily distinguishable from other blockchain addresses
- Supports both Tempo mainnet addresses and zone-specific addresses
- Includes strong checksumming to prevent user errors
- Uses Base58 encoding for compactness and readability

## Motivation

As Tempo expands with zones (L2 validiums), users need a clear way to distinguish:
1. Tempo mainnet addresses from other chains (Ethereum, Tron, etc.)
2. Zone addresses from Tempo mainnet addresses
3. Addresses on different zones from each other

A well-designed address format prevents costly mistakes like sending funds to the wrong chain or zone.

## Specification

### Address Structure

A Tempo address consists of a 2-character ASCII prefix followed by a Base58-encoded payload:

```
┌──────────────┬─────────────────────────────────────────────┐
│ ASCII Prefix │           Base58-encoded payload            │
│   2 chars    │                                             │
└──────────────┴─────────────────────────────────────────────┘
                              │
                              ▼
               ┌─────────┬──────────────┬──────────┐
               │ Zone ID │ Raw Address  │ Checksum │
               │ 0-9 B   │ 20 bytes     │ 4 bytes  │
               └─────────┴──────────────┴──────────┘
```

### ASCII Prefix (2 characters)

The prefix is **not** Base58-encoded; it is literal ASCII:

| Prefix | Meaning | Has Zone ID |
|--------|---------|-------------|
| `t1` | Tempo mainnet | No |
| `tz` | Tempo zone | Yes |
| `tt` | Tempo testnet | No |
| `tZ` | Tempo testnet zone | Yes |

### Zone ID Encoding (Variable Length, 0-9 bytes)

For zone addresses (prefixes `tz` and `tZ`), the zone ID is encoded in the payload using variable-length encoding:

- Zone IDs 0-252: 1 byte (direct encoding)
- Zone IDs 253-65535: 3 bytes (`0xFD` + 2 bytes little-endian)
- Zone IDs 65536-4294967295: 5 bytes (`0xFE` + 4 bytes little-endian)
- Zone IDs > 4294967295: 9 bytes (`0xFF` + 8 bytes little-endian)

For mainnet addresses (prefixes `t1` and `tt`), the zone ID is omitted (0 bytes).

### Reserved Zone IDs

- **Zone ID 0**: Reserved for potential future use (e.g., treating Tempo mainnet as "zone 0" for unified cross-zone addressing). The `ZoneFactory` assigns zone IDs starting at 1.
- **Zone IDs 1+**: Assigned sequentially by `ZoneFactory.createZone()`

### Raw Address

The raw address is the standard 20-byte Ethereum-style address (last 20 bytes of keccak256 of public key).

### Checksum

The checksum is the first 4 bytes of:
```
SHA256(SHA256(prefix_bytes || zone_id || raw_address))
```

Where `prefix_bytes` is the 2-byte ASCII prefix (e.g., `"t1"` = `0x7431`, `"tz"` = `0x747A`).

This double-SHA256 checksum (same as Bitcoin) provides strong error detection. Including the prefix in the checksum ensures that changing the prefix invalidates the address.

### Base58 Alphabet

Uses the Bitcoin Base58 alphabet (excludes 0, O, I, l to avoid confusion):
```
123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz
```

### Examples

#### Tempo Mainnet Address

Raw address: `0x742d35Cc6634C0532925a3b844Bc9e7595f2bD28`

Encoding:
1. ASCII prefix: `t1`
2. Zone ID: (none, mainnet)
3. Raw address: 20 bytes
4. Checksum: first 4 bytes of `SHA256(SHA256("t1" || address))`
5. Base58 encode the 24 bytes (address + checksum)
6. Prepend ASCII prefix

Result: `t1J7XcvK8mJ3xGdQN5rY4kHvL2nP6wT9ab` (example, ~34 chars total)

#### Zone Address (Zone ID = 1)

Raw address: `0x742d35Cc6634C0532925a3b844Bc9e7595f2bD28`

Encoding:
1. ASCII prefix: `tz`
2. Zone ID: `0x01` (1 byte for zone 1)
3. Raw address: 20 bytes
4. Checksum: first 4 bytes of `SHA256(SHA256("tz" || 0x01 || address))`
5. Base58 encode the 25 bytes (zone_id + address + checksum)
6. Prepend ASCII prefix

Result: `tzAJ7XcvK8mJ3xGdQN5rY4kHvL2nP6wT9ab` (example, ~36 chars total)

#### Zone Address (Zone ID = 1000)

Encoding:
1. ASCII prefix: `tz`
2. Zone ID: `0xFD 0xE8 0x03` (3 bytes for zone 1000)
3. Raw address: 20 bytes
4. Checksum: first 4 bytes of `SHA256(SHA256("tz" || zone_id || address))`
5. Base58 encode the 27 bytes (zone_id + address + checksum)
6. Prepend ASCII prefix

Result: `tzBXcvK8mJ3xGdQN5rY4kHvL2nP6wT9abcd` (example, ~39 chars total)

#### Zone Address (Zone ID = 2^64 - 1, maximum)

Encoding:
1. ASCII prefix: `tz`
2. Zone ID: `0xFF` + 8 bytes (9 bytes total for max uint64)
3. Raw address: 20 bytes
4. Checksum: 4 bytes
5. Base58 encode the 33 bytes

Result: ~47 characters total

### Address Length Summary

| Type | Payload Size | Total Length (approx) |
|------|-------------|----------------------|
| Mainnet (`t1`) | 24 bytes | 2 + 32-33 = **34-35 chars** |
| Zone (ID < 253) | 25 bytes | 2 + 33-34 = **35-36 chars** |
| Zone (ID < 65536) | 27 bytes | 2 + 36-37 = **38-39 chars** |
| Zone (ID < 4B) | 29 bytes | 2 + 39-40 = **41-42 chars** |
| Zone (ID max uint64) | 33 bytes | 2 + 44-45 = **46-47 chars** |

### Validation Rules

1. Verify address starts with valid prefix (`t1`, `tz`, `tt`, or `tZ`)
2. Extract and Base58-decode the payload (everything after the 2-char prefix)
3. If zone prefix (`tz` or `tZ`), decode zone ID using variable-length encoding
4. If mainnet prefix (`t1` or `tt`), verify no zone ID bytes present
5. Extract raw address (20 bytes)
6. Extract checksum (last 4 bytes)
7. Compute expected checksum: `SHA256(SHA256(prefix || zone_id || address))[0:4]`
8. Verify checksum matches
9. If any step fails, address is INVALID

### Display Format

For user interfaces, addresses SHOULD be displayed with:
- Monospace font
- Groups of 4-5 characters separated by spaces or thin spaces for readability
- Full address always shown (no truncation for important operations)

Example: `t1J7X cvK8m J3xGd QN5rY 4kHvL 2nP6w T9`

### Interoperability

#### From Raw Address

To convert a raw 20-byte address to Tempo format:

```python
def encode_tempo_address(raw_address: bytes, zone_id: int | None = None, testnet: bool = False) -> str:
    if zone_id is None:
        prefix = "tt" if testnet else "t1"
        zone_bytes = b''
    else:
        prefix = "tZ" if testnet else "tz"
        zone_bytes = encode_compact_size(zone_id)
    
    # Checksum includes prefix
    checksum_input = prefix.encode('ascii') + zone_bytes + raw_address
    checksum = sha256(sha256(checksum_input))[:4]
    
    # Base58 encode only the payload (zone_id + address + checksum)
    payload = zone_bytes + raw_address + checksum
    return prefix + base58_encode(payload)
```

#### To Raw Address

To extract the raw address from a Tempo address:

```python
def decode_tempo_address(address: str) -> tuple[bytes, int | None, bool]:
    """Returns (raw_address, zone_id, is_testnet)"""
    
    # Extract prefix
    prefix = address[:2]
    if prefix not in ("t1", "tz", "tt", "tZ"):
        raise InvalidPrefix(prefix)
    
    is_testnet = prefix in ("tt", "tZ")
    has_zone = prefix in ("tz", "tZ")
    
    # Decode payload
    payload = base58_decode(address[2:])
    
    # Parse zone ID if present
    if has_zone:
        zone_id, zone_len = decode_compact_size(payload)
        remaining = payload[zone_len:]
    else:
        zone_id = None
        remaining = payload
    
    # Extract address and checksum
    raw_address = remaining[:20]
    checksum = remaining[20:24]
    
    if len(remaining) != 24:
        raise InvalidLength()
    
    # Verify checksum
    if has_zone:
        zone_bytes = encode_compact_size(zone_id)
    else:
        zone_bytes = b''
    
    expected = sha256(sha256(prefix.encode() + zone_bytes + raw_address))[:4]
    if checksum != expected:
        raise InvalidChecksum()
    
    return raw_address, zone_id, is_testnet
```

## Rationale

### Why Separate ASCII Prefix?

- The prefix is always visible and consistent (not affected by Base58 encoding)
- Easy to identify chain/network at a glance without decoding
- Allows different prefixes for mainnet vs zones vs testnet
- Checksum includes prefix, so changing prefix invalidates the address

### Why Base58?

- More compact than hex (~34 chars vs 42 chars for mainnet)
- Avoids ambiguous characters (0/O, I/l)
- Well-established in cryptocurrency (Bitcoin, Tron)
- Easy to copy/paste and read aloud

### Why Double-SHA256 Checksum?

- Battle-tested in Bitcoin for 15+ years
- 4 bytes provides 1 in 4 billion chance of random collision
- Detects any single-character error
- Detects most transposition errors

### Why Variable-Length Zone ID (up to 9 bytes)?

- Most zones will have small IDs (< 253), using only 1 byte
- Supports full uint64 range (9 bytes max: 1 byte flag + 8 bytes value)
- Same encoding as Bitcoin CompactSize (well-understood)
- Balances compactness for common cases with flexibility for large deployments

### Why 't' Prefix?

- Lowercase 't' is visually distinct from Tron's uppercase 'T'
- Immediately identifies Tempo addresses
- `t1` for mainnet, `tz` for zones creates clear visual distinction

## Backwards Compatibility

This is a new addressing scheme. Raw 20-byte addresses remain valid for on-chain operations; this format is for user-facing display and input.

## Security Considerations

1. **Checksum Strength**: The 4-byte double-SHA256 checksum provides strong error detection but is not cryptographically secure against intentional collisions. Users should verify addresses through multiple channels for high-value transfers.

2. **Zone Verification**: Wallets MUST clearly display the zone ID when showing zone addresses to prevent cross-zone transfer errors.

3. **Prefix Confusion**: The `t` prefix is intentionally distinct from other chains. Wallets should reject addresses that decode successfully but have unexpected prefixes.

## Reference Implementation

See `tempo-sdk` for TypeScript implementation.

## Copyright

Copyright and related rights waived via CC0.
