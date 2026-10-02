#!/usr/bin/env python3
"""Check the isolated pre/post-T15 Zone deposit and batch settlement run."""

import argparse
import hashlib
import json
from importlib.machinery import SourceFileLoader
from pathlib import Path

baseline = SourceFileLoader(
    "native_payment_baseline", str(Path(__file__).with_name("check-baseline.py"))
).load_module()

TOKEN = baseline.TOKEN
PORTAL_IMPLEMENTATION = "0x5ad1000000000000000000000000000000000000"
VERIFIER = "0x5a56000000000000000000000000000000000000"


def token_amount(rpc_url, account, block):
    data = "0x70a08231" + account.removeprefix("0x").lower().rjust(64, "0")
    return int(baseline.rpc(rpc_url, "eth_call", {
        "to": TOKEN, "from": account, "data": data,
    }, hex(block)), 16)


def supply(rpc_url, block):
    return int(baseline.rpc(rpc_url, "eth_call", {
        "to": TOKEN, "data": "0x18160ddd",
    }, hex(block)), 16)


def checked_transaction(rpc_url, tx_hash, portal):
    receipt = baseline.receipt(rpc_url, tx_hash)
    block = baseline.rpc(rpc_url, "eth_getBlockByNumber", receipt["blockNumber"], False)
    baseline.require(block["transactions"] == [tx_hash], "transaction block is not isolated")
    tx = baseline.rpc(rpc_url, "eth_getTransactionByHash", tx_hash)
    call = tx["calls"][0] if "calls" in tx else tx
    baseline.require(call["to"].lower() == portal.lower(), "wrong portal target")
    return receipt, block, tx, call


def lane_sample(paths, timestamp, gas_used):
    for path in paths:
        for line in path.read_text().splitlines():
            sample = json.loads(line)
            # Metrics use wall time; the block timestamp can precede the sample by a second.
            if (0 <= sample["time"] - timestamp <= 2
                    and sample["pool_transactions_included_last"] == 1
                    and sample["payment_transactions_last"] == 1
                    and sample["payment_gas_used_last"] == gas_used
                    and sample["general_gas_used_last"] == 0):
                return sample
    raise RuntimeError(f"no zero-general-gas lane sample for {timestamp}, {gas_used}")


def check_deposit(rpc_url, tx_hash, portal, fork_time, after_fork, metrics):
    receipt, block, _, call = checked_transaction(rpc_url, tx_hash, portal)
    timestamp = int(block["timestamp"], 16)
    baseline.require((timestamp >= fork_time) == after_fork, "deposit on wrong fork side")
    baseline.require(call["input"][:10] == "0x03dd6f34", "wrong deposit selector")
    baseline.require(len(bytes.fromhex(call["input"][2:])) == 420,
                     "noncanonical deposit calldata")
    trace = baseline.rpc(rpc_url, "debug_traceTransaction", tx_hash,
                         {"tracer": "callTracer"})
    children = trace.get("calls", [])
    def descendants(frame):
        for child in frame.get("calls", []):
            yield child
            yield from descendants(child)
    baseline.require(any(child["type"] == "CALL" and child["to"].lower() == TOKEN
                         and child["input"][:10] == "0x23b872dd"
                         for child in descendants(trace)),
                     "deposit did not transfer TIP-20 backing")
    if after_fork:
        baseline.require(all(child["type"] == "CALL" for child in children),
                         "post-fork deposit used proxy delegatecall")
    gas_used = int(receipt["gasUsed"], 16)
    return {
        "hash": tx_hash, "block": int(receipt["blockNumber"], 16),
        "timestamp": timestamp, "gas_used": gas_used,
        "trace_children": [{"type": c["type"], "to": c["to"]} for c in children],
        "lane_sample": lane_sample(metrics, timestamp, gas_used) if after_fork else None,
    }


def check_batch(rpc_url, tx_hash, portal, fork_time, metrics, expected_cursor):
    receipt, block, tx, call = checked_transaction(rpc_url, tx_hash, portal)
    timestamp = int(block["timestamp"], 16)
    baseline.require(timestamp >= fork_time, "batch preceded T15")
    baseline.require("calls" in tx and len(tx["calls"]) == 1,
                     "expected one AA settlement call")
    baseline.require(call["input"][:10] == "0x4cd6c7c7", "wrong batch selector")
    baseline.require(baseline.dynamic_bytes(call["input"], 11) == b"\x02"
                     and baseline.dynamic_bytes(call["input"], 12) == b"",
                     "not explicit NoProof development settlement")
    calldata = bytes.fromhex(call["input"][10:])
    word = lambda n: int.from_bytes(calldata[32*n:32*(n+1)], "big")
    baseline.require((word(6), word(7)) == expected_cursor,
                     "wrong deposit cursor transition")
    trace = baseline.rpc(rpc_url, "debug_traceTransaction", tx_hash,
                         {"tracer": "callTracer"})
    children = trace.get("calls", [])
    baseline.require(any(c["type"] == "DELEGATECALL"
                         and c["to"].lower() == PORTAL_IMPLEMENTATION for c in children),
                     "native portal did not use bounded implementation frame")
    delegate = next(c for c in children if c["type"] == "DELEGATECALL"
                    and c["to"].lower() == PORTAL_IMPLEMENTATION)
    baseline.require(any(c["type"] == "STATICCALL" and c["to"].lower() == VERIFIER
                         for c in delegate.get("calls", [])),
                     "implementation did not call verifier")
    gas_used = int(receipt["gasUsed"], 16)
    return {
        "hash": tx_hash, "block": int(receipt["blockNumber"], 16),
        "timestamp": timestamp, "gas_used": gas_used,
        "anchor": word(0), "deposit_cursor_before": word(6),
        "deposit_cursor_after": word(7), "proof_mode": "NoProof (0x02)",
        "proof_length": 0, "delegate_target": delegate["to"],
        "verifier_target": VERIFIER,
        "lane_sample": lane_sample(metrics, timestamp, gas_used),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("l1-rpc-url", "zone-rpc-url", "portal", "account", "pre-deposit-tx",
                 "post-deposit-tx", "first-batch-tx", "second-batch-tx", "tempo-binary",
                 "zone-binary", "output"):
        parser.add_argument("--" + name, required=True)
    parser.add_argument("--fork-time", type=int, required=True)
    parser.add_argument("--pre-zone-block", type=int, required=True)
    parser.add_argument("--post-zone-block", type=int, required=True)
    parser.add_argument("--metrics", type=Path, nargs="+", required=True)
    args = parser.parse_args()
    require = baseline.require
    rpc = baseline.rpc
    pre = check_deposit(args.l1_rpc_url, args.pre_deposit_tx, args.portal,
                        args.fork_time, False, args.metrics)
    post = check_deposit(args.l1_rpc_url, args.post_deposit_tx, args.portal,
                         args.fork_time, True, args.metrics)
    first = check_batch(args.l1_rpc_url, args.first_batch_tx, args.portal,
                        args.fork_time, args.metrics, (1, 1))
    second = check_batch(args.l1_rpc_url, args.second_batch_tx, args.portal,
                         args.fork_time, args.metrics, (1, 2))
    require(pre["block"] < 176 < first["block"] < post["block"] < second["block"],
            "wrong fork/run transaction sequence")
    blocks = [rpc(args.l1_rpc_url, "eth_getBlockByNumber", hex(n), False)
              for n in (176, 177)]
    require(int(blocks[0]["timestamp"], 16) < args.fork_time
            <= int(blocks[1]["timestamp"], 16), "fork activation boundary mismatch")
    balance_pre = token_amount(args.zone_rpc_url, args.account, args.pre_zone_block)
    balance_post = token_amount(args.zone_rpc_url, args.account, args.post_zone_block)
    backing_pre = token_amount(args.l1_rpc_url, args.portal, pre["block"])
    backing_post = token_amount(args.l1_rpc_url, args.portal, post["block"])
    supply_pre = supply(args.zone_rpc_url, args.pre_zone_block)
    supply_post = supply(args.zone_rpc_url, args.post_zone_block)
    require((balance_pre, backing_pre, supply_pre) == (1_000_000,) * 3,
            "pre-fork liability/backing mismatch")
    require((balance_post, backing_post, supply_post) == (2_000_000,) * 3,
            "post-fork liability/backing mismatch")
    digest = lambda path: hashlib.sha256(Path(path).read_bytes()).hexdigest()
    evidence = {
        "kind": "t15_zone_settlement_fork_boundary_smoke",
        "full_native_payment_acceptance": False,
        "execution_proof_verified": False,
        "preexisting_zone_crossed_activation": True,
        "fork_time": args.fork_time,
        "activation_boundary": [
            {"number": n, "timestamp": int(b["timestamp"], 16), "hash": b["hash"]}
            for n, b in zip((176, 177), blocks)
        ],
        "clients": {"l1": rpc(args.l1_rpc_url, "web3_clientVersion"),
                    "zone": rpc(args.zone_rpc_url, "web3_clientVersion")},
        "genesis": {"l1": rpc(args.l1_rpc_url, "eth_getBlockByNumber", "0x0", False)["hash"],
                    "zone": rpc(args.zone_rpc_url, "eth_getBlockByNumber", "0x0", False)["hash"]},
        "binary_sha256": {"tempo": digest(args.tempo_binary),
                          "zones": digest(args.zone_binary)},
        "portal": args.portal, "account": args.account,
        "pre_fork": {"deposit": pre, "zone_balance_block": args.pre_zone_block,
                     "private_balance": balance_pre, "portal_backing": backing_pre,
                     "zone_supply": supply_pre},
        "post_fork": {"deposit": post, "first_batch": first, "second_batch": second,
                      "zone_balance_block": args.post_zone_block,
                      "private_balance": balance_post, "portal_backing": backing_post,
                      "zone_supply": supply_post},
    }
    Path(args.output).write_text(json.dumps(evidence, indent=2) + "\n")
    print(json.dumps({"fork_crossed": True, "payment_batches": 2,
                      "private_balance": balance_post, "portal_backing": backing_post}))


if __name__ == "__main__":
    main()
