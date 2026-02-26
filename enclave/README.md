# Nitro Enclave — Zone Sequencer

Run the zone sequencer inside an [AWS Nitro Enclave](https://aws.amazon.com/ec2/nitro/nitro-enclaves/) with reproducible builds, enabling on-chain attestation of the exact code running in the TEE.

## Architecture

```
┌───────────────────────────────────────────────────────┐
│                  EC2 Instance (parent)                 │
│                                                       │
│   run-enclave.sh      vsock-proxy.py                  │
│        │                   │                          │
│        │              vsock port 8000                  │
│        ▼                   │                          │
│   ┌────────────────────────┼──────────────────────┐   │
│   │           Nitro Enclave (isolated)            │   │
│   │                                               │   │
│   │   tempo-zone (zone sequencer)                 │   │
│   │       │                                       │   │
│   │       ├── Builds blocks from L1 deposits      │   │
│   │       ├── Signs outputs with enclave key      │   │
│   │       └── Communicates via vsock only         │   │
│   │                                               │   │
│   │   No network · No disk · No external access   │   │
│   └───────────────────────────────────────────────┘   │
│                        │                              │
│                   vsock proxy ──── L1 RPC (Tempo)     │
└───────────────────────────────────────────────────────┘
```

**Parent instance** runs the vsock proxy, which forwards L1 RPC traffic into the enclave over vsock. The parent has no access to the enclave's memory or keys.

**Nitro Enclave** is a fully isolated VM. It has no network interface, no persistent storage, and no access to the parent's memory. The only communication channel is vsock.

### PCR Measurements

Every Nitro Enclave has three Platform Configuration Register (PCR) values:

| PCR  | Description |
|------|-------------|
| PCR0 | Hash of the enclave image file (EIF) |
| PCR1 | Hash of the Linux kernel inside the enclave |
| PCR2 | Hash of the application (user-space binary + root filesystem) |

These values are deterministic — the same source code and build environment produce the same PCRs. This allows anyone to:

1. Build the enclave image from source
2. Compare their PCRs to the registered on-chain values
3. Verify the sequencer is running exactly the expected code

The `measurementHash` is `keccak256(PCR0 || PCR1 || PCR2)` — a single hash suitable for on-chain registration.

## Prerequisites

- **EC2 instance** with Nitro Enclave support (e.g., `m5.xlarge` or larger with `--enclave true`)
- **Docker** — for building the deterministic image
- **AWS Nitro CLI** — `nitro-cli` for building EIFs and managing enclaves
  - [Installation guide](https://docs.aws.amazon.com/enclaves/latest/user/nitro-enclave-cli-install.html)
- **Foundry** (`cast`) — for keccak256 hashing
- **jq** — for JSON processing

Install Nitro CLI on Amazon Linux 2 / AL2023:

```bash
sudo amazon-linux-extras install aws-nitro-enclaves-cli
sudo yum install aws-nitro-enclaves-cli-devel
sudo usermod -aG ne $USER
sudo systemctl enable --now nitro-enclaves-allocator
```

Configure enclave resources in `/etc/nitro_enclaves/allocator.yaml`:

```yaml
memory_mib: 4096
cpu_count: 2
```

## Building the Enclave Image

### 1. Build the EIF

```bash
# From the repository root:
./enclave/build-enclave.sh --output-dir enclave/out

# Or via Just:
just build-enclave
```

This produces:
- `enclave/out/tempo-zone.eif` — the Enclave Image File
- `enclave/out/measurements.json` — PCR values and metadata

### 2. Verify Reproducible Builds

To verify that a build is reproducible, build twice from the same commit and compare:

```bash
git checkout <commit>

./enclave/build-enclave.sh --output-dir /tmp/build-a
./enclave/build-enclave.sh --output-dir /tmp/build-b

diff <(jq -S . /tmp/build-a/measurements.json) <(jq -S . /tmp/build-b/measurements.json)
```

The PCR values and `measurementHash` must be identical. The `buildTimestamp` will differ but does not affect the EIF content.

### 3. View Measurements

```bash
# Print all measurements
cat enclave/out/measurements.json | jq .

# Print just the measurementHash (for on-chain registration)
just enclave-measurements
```

## Running the Enclave

### 1. Start the Enclave

```bash
./enclave/run-enclave.sh enclave/out/tempo-zone.eif --cpu-count 2 --memory 4096
```

This will:
1. Terminate any existing enclave
2. Start the enclave from the EIF
3. Launch the vsock proxy for L1 RPC access

### 2. View Console Output

```bash
nitro-cli console --enclave-id <enclave-id>
```

### 3. Terminate

```bash
nitro-cli terminate-enclave --enclave-id <enclave-id>
```

## vsock Proxy

The enclave has no network access. The `vsock-proxy.py` script runs on the parent instance and forwards traffic from the enclave to an external TCP endpoint (L1 RPC).

```bash
# Start manually (usually handled by run-enclave.sh)
python3 enclave/vsock-proxy.py \
    --vsock-port 8000 \
    --target-host rpc.moderato.tempo.xyz \
    --target-port 443
```

Inside the enclave, `tempo-zone` connects to `vsock://<parent-cid>:8000` to reach L1.

## Registering the Enclave Key On-Chain

After the enclave is running, register its `measurementHash` on-chain so that L1 contracts can verify attestations:

```bash
MEASUREMENT_HASH=$(jq -r '.measurementHash' enclave/out/measurements.json)

# Register via the zone portal (replace with actual contract call)
cast send "$PORTAL_ADDRESS" \
    "registerEnclaveMeasurement(bytes32)" \
    "$MEASUREMENT_HASH" \
    --rpc-url "$L1_RPC_URL" \
    --private-key "$PRIVATE_KEY"
```

## Rotating Keys

When updating the sequencer code:

1. Build a new EIF from the updated source
2. Compare PCR values to confirm the change
3. Register the new `measurementHash` on-chain
4. Deploy the new EIF to the enclave
5. The old measurement can be revoked after the transition period

## Troubleshooting

### `nitro-cli: command not found`

Install the Nitro CLI and ensure you're on a Nitro-capable EC2 instance.

### Enclave fails to start with memory errors

Increase the allocator memory in `/etc/nitro_enclaves/allocator.yaml` and restart:

```bash
sudo systemctl restart nitro-enclaves-allocator
```

### vsock proxy connection refused

Ensure the proxy is running on the parent instance and the vsock port matches what the enclave expects. Check with:

```bash
ss -tlnp | grep vsock
```

### Reproducibility check fails

Ensure:
- Same git commit (check `git rev-parse HEAD`)
- Same Docker version (`docker --version`)
- Same `nitro-cli` version (`nitro-cli --version`)
- No local modifications (`git status` is clean)

### Console shows no output

The enclave takes a few seconds to boot. Wait and retry:

```bash
nitro-cli console --enclave-id <id>
```

If still empty, the binary may be crashing. Rebuild with debug logging enabled.
