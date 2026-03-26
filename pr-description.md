This PR updates the L1 subscriber logic to simplify the block processing flow, remove the 1-block confirmation buffer that added ~500ms of unnecessary latency to intra-zone transfers, and consolidate redundant reorg detection and cache locking. The following has been updated:

- Removed the 1-block confirmation buffer — blocks are now applied immediately as they arrive. L1 has fast finality so buffering provided no reorg safety while adding a full block of latency before deposits landed on the zone.
- `sync_to_l1_tip` returns the tip block hash, which seeds a `parent_hash` tracker in the new `handle_l1_block_stream` method for straightforward reorg detection.
- Merged `update_l1_state_anchor` and `apply_portal_state_events` into `update_l1_state_cache` — single write lock per block instead of two.
- Removed duplicate reorg detection from `update_l1_state_anchor` — now handled once at the stream level.
- Removed `listener_events_applied` metric — not actionable, cache gauges are sufficient.

Note: reorg handling still only clears the L1 state cache — full unwind of applied state is a separate follow-up.
