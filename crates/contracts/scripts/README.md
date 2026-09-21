# Contract compilation and size checks

Run from `crates/contracts`:

```sh
forge build --skip test
python3 -m unittest discover -s scripts -p 'test_*.py'
python3 scripts/check_sizes.py
```

Tempo installs ZonePortal, ZoneMessenger, and Verifier runtime bytecode directly
at protocol addresses through its hardfork state hooks. These implementations
do not pass through CREATE, so their byte sizes are informational. The checker
identifies exemptions by both source path and contract name, reports byte lengths
and SHA-256 hashes, and retains EIP-170/EIP-3860 checks for ordinary production
contracts. Constructor arguments also count toward initcode limits and must be
accounted for by deployment tooling; the artifact report measures the compiler's
creation bytecode only. Compile fresh artifacts before running the checker.

The ABI and storage-layout checks still consume compiler artifacts and remain
required. No compiler optimization settings or EVM code-size limits are changed.

`PortalRuntimeTest` loads the compiled deployed runtime without constructing an
implementation, then installs the same 45-byte proxy used by Tempo's factory.
Behavior tests initialize proxy storage with Solidity `initialize`; this is a
fixture approximation of the native factory's Rust storage writes. Native
factory/hardfork integration coverage lives in Tempo. Creating another proxy
does not overwrite an existing implementation, allowing upgrade tests to retain
the selected runtime. The oversized-runtime regression pads real portal code
and executes it via the proxy; it does not claim to execute Tempo's hardfork hook.
