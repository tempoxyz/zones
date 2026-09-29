# Prover E2E trials

The publisher tests an exact commit without executing its code in the publisher
job. Argo builds the candidate against an isolated genesis, runs the selected
plans on one devnet, cleans up, then reports `Tempo Zone Prover E2E (experimental)`.
Do not add this context to required checks yet.

Deploy and verify the dedicated sensor and the complete pinned workflow template
set before enabling `PROVER_E2E_TRIALS_ENABLED=true`. This variable is unset by
default, so even manual dispatch is initially disabled. Manual dispatch requires
a full lowercase commit SHA and runs settlement, fallback, and short recovery.

Enable automatic entrypoints separately, only after a successful manual trial:

- `PROVER_E2E_MERGE_QUEUE_ENABLED=true`: settlement and fallback on the exact
  merge-group SHA.
- `PROVER_E2E_NIGHTLY_ENABLED=true`: settlement and short recovery on the resolved
  `main` SHA, daily at 08:40 UTC. Full ancestry remains excluded.

No Slack notifications or reviewer tags are emitted. The event carries only the
candidate SHA, trigger, and Actions run/attempt. Source revisions, plans, and quiet
mode are controlled by the sensor. Repeated deliveries use the same Workflow name.

For the first trials, verify the Workflow exists after event delivery and watch
the status reach a terminal state. HTTP delivery alone is not a submission
acknowledgment; a sensor failure can leave a pending status. The publisher has a
five-minute dispatch budget and the sensor a ninety-minute workflow deadline.
Neither is an agreed merge-queue runtime budget. The callback checks ownership
before updating a status, following the existing non-atomic read/write pattern.

The workflow code revision is pinned separately from its ClusterWorkflowTemplates;
both must match for a reproducible trial. The temporary Tempo base includes the
quiet-build guard. Successful CI workflows and their archived artifacts expire
after seven days. Failed or errored CI workflows, PVCs, and artifacts are retained
for 30 days for recovery.
Unresolved cleanup must be handled within that window; it is not indefinite storage.
Candidate dependency selection, image/genesis retention, repeated-run
runtime measurements, and submission-timeout handling remain rollout prerequisites
before making the suite required.

Observed preview timings (September 29, 2026):

| Work | Observed wall time |
| --- | --- |
| Fresh candidate/prover and PCR-patched L1 preparation | 23m22s |
| Distinct old + candidate prover and PCR-patched L1 preparation | 27m38s–33m41s |
| Warm-fixture settlement/fallback/recovery, provisioning and cleanup | 14m01s–15m19s |
| Individual settlement / fallback / short recovery | 11s–55s / 3m37s–4m33s / 1m58s–2m01s |

These are a few successful preview runs, not percentile guarantees or a queue
latency budget. Fork runs also wait for their scheduled activation. Keep the
experimental check nonrequired until repeated candidate trials establish a
suitable merge-queue budget. Build/cache optimization is deferred.
