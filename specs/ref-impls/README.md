# Zone runtime contracts

This directory contains the Solidity contracts that remain deployed as shared Tempo Zone
runtimes: `ZonePortal`, `ZoneMessenger`, and `Verifier`. Their compiled runtime bytecode is
synchronized into the Tempo repository by the `sync-tempo-zone-runtimes` workflow.

Run the contract tests with a Tempo-capable Foundry build:

```bash
forge test
```
