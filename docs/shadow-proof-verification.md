# Shadow Nitro proof verification

RPC followers can authenticate the Nitro proofs returned by a remote prover before T13.
This calls `ZoneVerifier::verify_with_pcrs` directly in `tempo-precompiles`,
without making an L1 `eth_call` or enabling proof-gated settlement. The legacy Solidity
verifier may return `true` without checking a proof, so it is not a substitute for this check.

Run a matching version of the node and attestation-capable enclave. Older SPF-only prover
images do not return the required `proofBundle`; the current protocol uses CBOR frames.
Build and approve the enclave EIF, then obtain PCR0, PCR1 and PCR2 from that build's
measurements artifact. Do not derive the allowlist from a prover response.

On the manifest's `rpc_only` follower, add:

```sh
--sequencer.enable-prover \
--sequencer.prover-address=<nitro-host>:5000 \
--shadow-prover.pcrs=<PCR0>,<PCR1>,<PCR2>
```

Each measurement must be exactly 48 bytes of hex (an optional `0x` prefix is accepted).
All three must be nonzero; debug-enclave measurements are rejected. `SHADOW_PROVER_PCRS`
is the equivalent environment setting. Omitting the setting preserves SPF-only observation;
enabling it requires the remote prover and is rejected on sequencing nodes.

After comparing the SPF output with the finalized batch, the worker authenticates the AWS
certificate chain and COSE signature, checks the pinned PCRs and time policy, and binds the
attestation to every batch commitment and the configured parent-chain/portal domain. It uses
the current L1 header timestamp fetched after proof generation, not the historical batch time.
Verification runs on a blocking worker with a 30-million-gas verification budget. A rejected
proof cannot count as a successful validation; an RPC or worker error is recorded separately.

The following metrics use the `reth_tempo_zone_prover_` prefix:

- `proof_verification_success_total`: authenticated shadow proofs.
- `proof_verification_failure_total`: proofs rejected by the verifier.
- `proof_verification_error_total`: verification interrupted by RPC, budget, or worker errors.
- `proof_verification_duration_seconds`: verification latency, including the time lookup.

Startup logs include `shadow_proof_verification=true`; a successful batch logs
`Shadow Nitro proof verified against pinned enclave measurements`. Confirm these metrics
advance on the deployed follower, and exercise deposits, user transactions and withdrawals
before treating an empty-batch run as coverage of those paths.

This local allowlist does not populate Tempo's consensus `APPROVED_PCRS`, activate T13,
or change the portal's settlement policy. No deployment or enclave image approval is
performed by this code change.
