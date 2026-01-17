# tempo-zone-sp1-guest

SP1 guest program for Tempo Zone batch proving.

This program runs inside the SP1 zkVM to generate zero-knowledge proofs for L2 batch transitions.

## Building

This crate is excluded from the main workspace because it targets the SP1 zkVM (`riscv32im-succinct-zkvm-elf`) and requires special tooling.

### Prerequisites

Install the SP1 toolchain:

```bash
curl -L https://sp1up.succinct.xyz | bash
sp1up
```

### Build

From this directory:

```bash
cargo prove build
```

Or from a prover crate that depends on this guest program:

```rust
// In build.rs
sp1_build::build_program("../tempo-zone-sp1-guest");
```

## Features

- `mock` - Skip state transition validation (for development/testing)

## Architecture

The guest program:

1. Reads `BatchInput` from the prover
2. Validates deposit queue transitions
3. Validates state transitions (or mocks in dev mode)
4. Computes withdrawal queue hashes
5. Commits `PublicValues` for on-chain verification

### Modules

- `types.rs` - Shared types matching the prover and Solidity contracts
- `deposit_hash.rs` - Deposit queue hash chain computation
- `withdrawal_hash.rs` - Withdrawal queue hash chain computation  
- `state.rs` - State transition validation (stateless EVM in production)

## Testing

Unit tests can be run on the host (not in zkVM):

```bash
cargo test
```

Note: The `main` function and SP1 entrypoint are only compiled for the zkVM target.
