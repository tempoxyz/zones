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
is the equivalent environment setting. With a remote prover and no PCR override, the shared
prover worker checks the proof through an L1 `eth_call` to the portal's current verifier after
T13. Before T13 that check is explicitly skipped. In-process SPF execution produces no proof
and remains output-only validation. Enabling PCR overrides requires a remote prover and is
rejected on sequencing nodes.

With PCR overrides, after comparing the SPF output with the finalized batch, the worker
authenticates the AWS certificate chain and COSE signature, checks the pinned PCRs and time
policy, and binds the attestation to every batch commitment and the configured
parent-chain/portal domain. Local verification uses the machine wall clock for certificate
and attestation time checks and does not require an L1 timestamp lookup.
Verification runs on a blocking worker. The in-memory storage provider does not meter gas.
A rejected proof cannot count as a successful validation; an RPC or worker error is recorded
separately.

The following metrics use the `reth_tempo_zone_prover_` prefix:

- `proof_verification_success_total`: proofs accepted by the local or L1 verifier.
- `proof_verification_failure_total`: proofs rejected by the verifier.
- `proof_verification_error_total`: verification interrupted by RPC, budget, or worker errors.
- `proof_verification_duration_seconds`: verification latency, including L1 lookups.
- `proof_verification_skipped_total`: remote proof checks skipped before T13 without a PCR override.

Startup logs include `shadow_proof_verification=true`; a successful batch logs
`Nitro proof verified` with `local=true`. Confirm these metrics advance on the deployed
follower, and exercise deposits, user transactions and withdrawals before treating an
empty-batch run as coverage of those paths.

This local allowlist does not populate Tempo's consensus `APPROVED_PCRS`, activate T13,
or change the portal's settlement policy. Settlement uses the same proving-and-verification
worker with no PCR override, so it checks the deployed L1 policy. The settlement monitor
chooses `NoProof` on any proving or verification failure, including fork and verifier-address
lookup failures. Fallback still requires a fresh `NoProof` certificate; certificate collection
and submission errors propagate normally.
No deployment or enclave image approval is performed by this code change.
