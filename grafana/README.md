# Grafana dashboards

## `zone-multisequencer.json`

Dashboard illustrating a multi-node zone sequencer set (leader + followers),
built against the metrics exposed by the zone node's reth metrics endpoint
(`--metrics`, port 6060 in the `tempo-zone` Helm chart).

Sections:

- **Sequencer set overview** — nodes reporting, leader head, follower
  replication lag (`reth_blockchain_tree_canonical_chain_height` per pod),
  block production rate.
- **Leader → L1 settlement** — `reth_tempo_zone_monitor_*`: observed vs
  submitted zone blocks, submission lag, batch submit rate/latency/size,
  withdrawals per batch, failure counters.
- **L1 ingestion (all nodes)** — `reth_tempo_zone_l1_subscriber_*`: L1 lag,
  latest L1 block seen, fetch failures/reconnects, deposit events.
- **Withdrawal processor** — `reth_tempo_zone_withdrawal_processor_*`.
- **Zone P2P** — `reth_zone_p2p_*` counters (transactions forwarded to the
  leader, role-invalid drops). These counters only appear after the first
  event, so panels may show "no data" on a quiet zone.
- **Node vitals** — CPU, memory, uptime per pod.

### Importing

Import the JSON via Grafana → Dashboards → Import and pick a Prometheus
datasource. The dashboard is parameterized by `namespace` and `pod` labels,
e.g. namespace `tempo-zone-unstable-multiseq` with pods
`zone-unstable-multiseq-leader-0`, `zone-unstable-multiseq-follower-a-0`,
`zone-unstable-multiseq-follower-b-0`.

### Prerequisite: scraping

The dashboard assumes the zone pods' metrics port (`reth-metrics`, 6060) is
scraped by Prometheus with standard Kubernetes labels (`namespace`, `pod`).
The `tempo-zone` chart does not ship a `PodMonitor` yet, so one must exist in
the target cluster for panels to populate.

Known gaps (candidates for new metrics): leader→follower block streaming,
settlement attestation requests/quorum, and sequencer-set version are not yet
instrumented, so the dashboard illustrates replication via per-node chain
height rather than attestation activity.
