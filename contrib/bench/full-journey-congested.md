# Full journey with L1 congestion

Select `full-journey-congested` in the existing Zones benchmark workflow's
`preset` input. It runs `private-flow-scenario.yml`: encrypted deposit, Earn
deposit, Earn redemption, plain L1 withdrawal. `full-journey` retains its existing
rate, fail-fast policy, timeouts and completion checks.

Provisioning, approvals, admission funding, authentication and Earn warmup all
finish before measurement. The initial schedule is in
`full-journey-congested.py`, with the traffic template in `neobank/l1-congestion.yml`:

| Phase | Duration | Background |
| --- | --- | --- |
| Uncongested | 60 seconds | None |
| Congested | 60 seconds | 1,000 submissions/s, 2,000,000 gas each |
| Expiry | 5 seconds plus a 1-second boundary allowance | Sending stopped; pending traffic expires |
| Recovery | At least 60 seconds, continuing until journeys finish | None |

Txgen already supports native Tempo transactions, ABI arguments, expiring
nonces and a paced `bench send` pipeline. The background calls the modexp
precompile (`0x…05`) with an ABI selector that declares an infeasible base length.
These deliberately failing calls consume gas without deploying a contract or
growing contract storage. `abis/congestion.json` only describes calldata; it is
not a deployed fixture. A seeded random argument makes each transaction unique.
This tests gas admission pressure, not CPU-intensive execution.

Mnemonic index 5 is already funded by the L1 snapshot and is separate from the
control/sequencer accounts (0–4) and journey accounts (16 onward). The background
pays a 1 gwei priority fee, above the journeys' zero tip, and expires after five
seconds. The runner checks fee headroom before starting. Both generator and
sender run in owned process groups and are terminated on success, failure,
timeout or cancellation. Expiry bounds traffic already accepted into the pool;
recovery excludes that interval. Background reports never enter journey JSON or
ClickHouse results.

The requested journey rate is a ceiling: starts are paced at
`min(tps, (count - 1) / 185)` so even fast journeys cannot consume the count before
recovery. The configured maximum in flight still applies; stalled journeys can
delay starts, but the independent congestion timer stops traffic regardless.
Only this preset uses `continue` after individual journey failures. In particular,
the existing 25-second encrypted deposits can expire, and their 45-second waits
can fail under congestion. These failures remain visible in the usual report.
The existing 10-minute journey timeout bounds individual stalls. The preset
requires a default step timeout of 3–10 minutes, count 20–10,000, and tps ≥ 0.1;
it also rejects pacing that leaves less than 10 minutes within its 20-minute
overall limit. Explicit step timeouts in the original scenario stay in force.

`congestion/blocks.jsonl` records every observed included block, background
transaction hashes and receipt gas, and remaining usable general capacity bounds.
Receipt gas alone can include state charges and refunds that differ from Tempo
block-capacity gas. The conservative background capacity lower bound is
`max(0, block.gasUsed - sum(other transactions' gas limits))`. The remaining
capacity upper bound subtracts this from
`min(mainBlockGeneralGasLimit, gasLimit - sharedGasLimit)`. Other activity can only
reduce the available capacity further. The topology's payload builder includes
these non-payment calls in the main block. Missing receipts, inconsistent block
hashes, unexpected calls and gas-limit mismatches invalidate the measurement.

After a five-second ramp, at least 80% of congested blocks must have a remaining
capacity upper bound ≤ 2,000,000 gas: room for at most one encrypted onramp's
declared gas, often zero; Earn withdrawal batches reserve more. Submitted traffic
does not satisfy this check. The run also requires an uncongested completion,
journeys overlapping congestion, all requested terminal outcomes, and a journey
started during recovery that completes. Individual failures do not invalidate
the experiment if those checks pass. Background inclusion during baseline or
after expiry fails the run.

The workflow appends phase starts, completions, failures, in-flight counts,
verified gas pressure, and time to the first recovery completion to the existing
summary. `congestion/report.json` contains phase timestamps and bounds. Existing
txgen reports supply journey and step latency; all journey lifecycles are retained
for correlation with those timestamps, including journeys stalled across phases.

Local checks:

```sh
python3 -m unittest discover -s contrib/bench/tests -p 'test_*.py'
bash -n contrib/bench/run-neobank-private-flow.sh contrib/bench/provision-topology.sh
```

Offline checks and an isolated L1 traffic smoke test do not validate the complete
Zone/Earn journey. An actual benchmark run requires the workflow's provisioned
two-validator L1, private Zone, Earn fixtures and reporting services.
