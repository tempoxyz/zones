# Experimental TDX prover

The initial TDX implementation runs the existing Zone SPF service in a Linux x86_64
TDX VM, produces Intel DCAP quote-v4 evidence, and authenticates connections before
sending witnesses. It is an experimental guest/local-verification path. **The pinned
Tempo verifier does not accept TDX settlement.** No production hardfork or measurement
policy is added by this change.

## Run the initial implementation

Build the same service and utility used for Nitro:

```bash
cargo build --release -p tempo-zone-prover-enclave -p tempo-zone-prover-utils
```

Inside the measured TDX guest, install Intel's `libtdx_attest.so.1`, configure access
to the host QGS, and run:

```bash
tempo-zone-prover-enclave --backend tdx --port 5000
```

The default listener is guest AF_VSOCK. A host proxy can forward a TCP endpoint to
it. `--use-tcp` listens on guest localhost, so exposing that mode requires a guest
forwarder. The host must never terminate the attested TLS session.

Before generating a TLS key, the service requires guest kernel arguments
`random.trust_bootloader=off random.trust_cpu=on` and no active hardware RNG driver.
Use CPU entropy; do not credit host-provided bootloader or virtio RNG input. Boot
arguments, kernel, root filesystem, prover executable, DCAP libraries, custom
Tempo genesis files, and startup configuration must be covered by an independently
approved measured-boot policy. An ordinary mutable VM with only firmware pinned
is insufficient: guest root can request arbitrary report data. Disable guest shell,
SSH/admin access, unmeasured executable loading, and writable executable/configuration
paths in the approved image. This repository does not yet build that VM image.

On the trusted client, install `libsgx_dcap_quoteverify.so.1` and the Intel quote
provider with PCCS/PCS access. Verification uses local QVL, accepts only an OK TCB
result and unexpired collateral, and rejects debug TDs. The native libraries are
loaded at runtime; missing libraries or quote-generation failures fail closed.

Create a policy file from independently approved build measurements:

```json
{
  "backend": "tdx",
  "measurements": [{
    "mr_td": "<48-byte hex measurement>",
    "mr_config_id": "<48-byte hex measurement>",
    "mr_owner": "<48-byte hex measurement>",
    "mr_owner_config": "<48-byte hex measurement>",
    "rtmrs": ["<RTMR0>", "<RTMR1>", "<RTMR2>", "<RTMR3>"],
    "td_attributes": 0,
    "xfam": 3
  }]
}
```

Replace all placeholders, attributes, and XFAM with the approved deployment's
values. These numbers are examples, not a deployment policy. Full tuples are
allowlisted: fields cannot be mixed between releases. Zero MRTD, empty allowlists,
unknown backend names, unknown fields, and debug policies are rejected. Zero owner
or unused runtime fields can be valid, but must match the approved tuple exactly.

Use the existing `prove --attestation-policy <file>` utility against the host proxy
with the original witness. It saves the SPF output and TDX proof bundle. The existing
RPC `verify` command targets L1 and cannot validate TDX with the current Tempo pin.
For local batch verification, use `zone_prover::tdx::Policy::verify_batch` with the
original public inputs, returned output/bundle, and trusted current Unix time.
Attested TLS authenticates the guest and channel; batch verification separately
checks the output's quote binding.

## First hardware test on Google Cloud

Google Cloud supports Intel TDX on C3 and C4 Confidential VMs. A C3 VM in
`us-central1-a` is a candidate for a short-lived test, subject to project quota
and available capacity. Azure DCesv6 is another option. This is a proposed test
procedure; the implementation has not been exercised on either cloud.

Use a Google Cloud project with billing and Compute Engine enabled. Select an
image marked `TDX_CAPABLE` rather than assuming an arbitrary Linux image supports
TDX. Check the current supported configurations before provisioning:

```bash
gcloud compute images list --filter='guestOsFeatures[].type:TDX_CAPABLE'
```

After selecting an image, set `TDX_IMAGE_PROJECT` and `TDX_IMAGE_NAME` and create
the test VM:

```bash
gcloud compute instances create zones-tdx-test \
  --zone=us-central1-a \
  --machine-type=c3-standard-4 \
  --confidential-compute-type=TDX \
  --maintenance-policy=TERMINATE \
  --image-project="$TDX_IMAGE_PROJECT" \
  --image="$TDX_IMAGE_NAME"
```

First use Google's documented ConfigFS/go-tdx-guest procedure to obtain a raw
quote with chosen report data. Confirm the returned format is quote v4; this
initial implementation rejects v5 and would need an explicit parser extension
if the selected platform returns it. Then configure a recent Intel attestation
library for the guest's supported quote interface (ConfigFS where available),
and install DCAP verification and quote-provider libraries on a trusted client.
Cloud providers manage the host quoting service; direct access to the host's
QGS/vsock listener should not be assumed.

Configure the guest boot arguments and RNG as required above, then use
`--backend tdx --use-tcp` and an SSH port forward to guest localhost:5000 for
the laboratory test. Exercise a fresh attested TLS connection, one valid witness,
independent batch-proof verification, tampered proof/report data, and mismatched
measurement policy. Replay the same witness on Nitro and compare the settlement
outputs. Record quote version, DCAP/TCB results, latency and peak memory.

An SSH-accessible mutable laboratory VM is useful for compatibility testing but
is not an approved prover deployment. Use synthetic witnesses; observed test
measurements are not a production allowlist. The next deployment milestone is
a reproducible immutable image with independently approved measurements.
Delete the temporary VM and its test disks after collecting results.

## Evidence and batch binding

TDX uses a separate bootstrap magic (`TZTDX001`) so clients cannot silently negotiate
a different trust backend. The guest produces a fresh quote with 64-byte report data:

- Bytes 0–31: SHA-256 of the existing TLS binding context and guest certificate.
- Bytes 32–63: the client's random 32-byte challenge.

The client verifies quote signature/collateral and the entire deployment identity
before trusting the certificate for TLS 1.3. A successful TLS handshake proves
possession of the attested key. Session resumption is disabled. TDX quotes have no
Nitro-style signed timestamp; the fresh challenge supplies connection freshness.
The policy therefore has no `max_age_seconds` field. Collateral expiration is checked
against the client's clock. DCAP work runs outside the async executor; the existing
handshake deadline bounds the connection, though a timed-out native call can finish
later in its worker thread.

After SPF replay, the guest emits an experimental proof bundle:

- `verifierConfig = 0x03` (a provisional allocation, not a ratified Tempo mode).
- `proof = raw Intel ECDSA P-256 TDX quote v4` (bounded to 64 KiB).
- Report data bytes 0–31 contain the batch digest; bytes 32–63 must be zero.

The digest uses the existing `NitroBatchAttestation` EIP-712 struct schema, including
all existing chain/verifier/zone, anchor, block, deposit, token-enablement, and
withdrawal commitments. Its `verifierConfigHash` is `keccak256(0x03)`. Retaining the
legacy type name avoids an unnecessary settlement-schema fork; the distinct config
hash separates Nitro and TDX digests. Nitro golden vectors still apply to `0x01`.
Unknown quote versions, SGX evidence, other signing algorithms, malformed signature
lengths, extra bytes, debug attributes, measurement mismatches, and report-data
mismatches are rejected. Parsing claims alone never authenticates a quote.

## Existing Nitro implementation and remaining work

The dependency is Tempo revision `9de35499af7dd84c889fae8edbf8d0db0331b8eb`.
The relevant upstream pieces are:

| Area | Current Nitro implementation | Work needed for TDX |
| --- | --- | --- |
| Protocol | TIP-1091 portal/factory domain; TIP-1096 multi-block inputs; TIP-1098 native Nitro verification at T13 | Draft a TDX TIP/amendment with allocated mode, quote versions, collateral envelope, policy, gas and activation rules. Do not infer an activation fork from T13. |
| L1 dispatch | Tempo `crates/precompiles/src/zone_verifier/{mod,dispatch,attestation}.rs`; shared verifier address; Nitro document/calldata bounds | Add a hardfork-gated TDX branch and quote/collateral bounds at the same address. Reconstruct chain ID and verifier address from execution context and require the canonical portal before parsing. |
| Cryptography | Tempo `crates/nitro-attestation`; bounded CBOR/COSE, AWS certificate chain, SHA-384/P-384 | Add bounded quote parsing, P-256 quote/QE/PCK verification, Intel root chain, CRLs, QE identity and platform/TDX TCB checks. Specify supported Intel module identities and SVN policy. |
| Collateral | AWS document includes its certificate chain; root and PCRs are fork policy | Define a self-contained collateral envelope or consensus-controlled collateral registry, expiration/revocation rules and updates. Validators must never fetch PCCS/PCS or use wall-clock time during execution. Use block time and protocol-pinned inputs. Local DCAP QVL is not the precompile implementation. |
| Measurement policy | Hardfork-indexed PCR0/1/2 allowlist; restricted development overrides | Hardfork-indexed MRTD/RTMR/configuration/attributes/XFAM policy from reproducible approved builds; measured boot must cover the full trusted guest software and configuration. |
| Gas and limits | TIP-1098 common input/document/crypto charges and dispatcher caps | Charge quote bytes, collateral bytes, certificate/signature work and TCB/CRL processing before expensive work. Define malformed-input gas behavior and denial-of-service limits. |
| Zone settlement | `VerifierMode`, sequencer `prover.rs`, `monitor.rs`, settlement manager, follower/quorum certificates | Add the activated TDX mode, select the mode before quorum signing, route to an approved backend by fork, validate returned mode, and propagate its config hash through certificates and settlement. Current code intentionally rejects `0x03`. |
| Observation/tooling | Nitro transport policy, `ShadowProofVerifier`, prover utility ABI calls and fork readiness checks | Extend CLI shadow verification and readiness for TDX, verify batch quotes after transport, and distinguish transport, SPF, collateral, policy and L1 errors. |
| Deployment | Nitro EIF/kernel/NSM Docker build, host allocator/`nitro-cli`, vsock proxy and release workflows | Reproducible measured VM image and boot chain, QGS/PCCS operations, CPU entropy configuration, immutable guest launch, exact build-to-measurement manifest, artifact signing and release workflows. EIF PCRs cannot be translated into TDX measurements. |
| Verification | Nitro fixtures, parser fuzzing, native verifier gas tests, settlement/fork integration tests | Real signed TDX fixtures and hardware smoke tests, quote/envelope fuzzing, certificate/CRL/TCB adversarial tests, gas tests, fork gating and complete settlement/cross-backend replay tests. |

The Solidity `runtime/tempo/Verifier.sol` is a permissive prototype; replacing it
would not implement the native post-T13 verifier. Update the Tempo implementation
and dependency pin together with activation tests. The current spec's references to
TIP-1096 as the Nitro activation source are stale; TIP-1098 is the actual native
Nitro verification TIP in the pinned Tempo source.

## Combining Nitro and TDX

Both backends can share one service and one L1 verifier interface. This initial
implementation already shares chain configuration, witness framing, SPF replay,
response handling, TLS key generation, and batch commitment construction. Keep
hardware evidence parsing, roots, measurement policy, freshness semantics and guest
launch/entropy configuration backend-specific.

The next integration should introduce an attestation-backend abstraction with
`mode`, `attest_batch`, and `attest_transport` operations, paired with an explicit
client verification policy. Keep the backend immutable per guest process. Use a
shared commitment schema and different committed config hashes so evidence cannot
be substituted across policies. L1 can dispatch internally from `verifierConfig`
without changing the portal ABI or adding an application-facing precompile.

Supporting either backend is an OR policy: compromise of either approved backend
can authorize a forged transition. Requiring both is a separate AND policy with
a new config value and a bounded two-evidence envelope. Both quotes must bind the
same output and the combined config hash; concatenating today's independently
bound `0x01` and `0x03` proofs is insufficient. Specify independent replay, failure
handling and quorum mode selection for that policy. Do not silently downgrade from
AND to either backend or to proofless fallback. Dual proving also doubles replay
work and makes availability depend on both platforms.

## Source references

- [Pinned TIP-1098](https://github.com/tempoxyz/tempo/blob/9de35499af7dd84c889fae8edbf8d0db0331b8eb/tips/tip-1098.md)
- [Intel quote-v4 layout](https://github.com/intel/confidential-computing.sgx.sdk/blob/main/common/inc/sgx_quote_4.h)
- [Intel quote generation API](https://github.com/intel/confidential-computing.tee.dcap/blob/main/QuoteGeneration/quote_wrapper/tdx_attest/tdx_attest.h)
- [Intel quote verification API](https://github.com/intel/confidential-computing.tee.dcap/blob/main/QuoteVerification/dcap_quoteverify/inc/sgx_dcap_quoteverify.h)

- [Google Cloud TDX configurations](https://docs.cloud.google.com/confidential-computing/confidential-vm/docs/supported-configurations?tab=intel-tdx)
- [Google Cloud Confidential VM creation](https://docs.cloud.google.com/confidential-computing/confidential-vm/docs/create-a-confidential-vm-instance)
- [Google Cloud raw TDX attestation procedure](https://docs.cloud.google.com/confidential-computing/confidential-vm/docs/tdx-provenance)
- [Azure DCesv6](https://learn.microsoft.com/en-us/azure/virtual-machines/sizes/general-purpose/dcesv6-series)
