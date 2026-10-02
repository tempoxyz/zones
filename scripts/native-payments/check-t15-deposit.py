#!/usr/bin/env python3
"""Verify the isolated T15 native-deposit smoke test; not full payment acceptance."""
import argparse
import json
from pathlib import Path

from importlib.machinery import SourceFileLoader

baseline = SourceFileLoader(
    "native_payment_baseline", str(Path(__file__).with_name("check-baseline.py"))
).load_module()

TOKEN = baseline.TOKEN
TRANSFER_FROM = "0x23b872dd"
DEPOSIT_SELECTORS = {"0x03dd6f34"}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in (
        "l1-rpc-url", "zone-rpc-url", "portal", "account", "deposit-tx",
        "settlement-tx", "output",
    ):
        parser.add_argument("--" + name, required=True)
    parser.add_argument("--fork-time", required=True, type=int)
    parser.add_argument("--amount", required=True, type=int)
    parser.add_argument("--expected-total", type=int)
    parser.add_argument("--cursor-before", type=int, default=0)
    parser.add_argument("--lane-metrics-file", type=Path)
    parser.add_argument("--forged-tx")
    parser.add_argument("--forged-portal")
    parser.add_argument("--forged-metrics-file", type=Path)
    parser.add_argument("--l1-balance-block", type=int)
    parser.add_argument("--zone-balance-block", type=int)
    parser.add_argument("--tempo-revision", required=True)
    parser.add_argument("--zones-revision", required=True)
    parser.add_argument("--tempo-binary-sha256", required=True)
    parser.add_argument("--zone-binary-sha256", required=True)
    args = parser.parse_args()
    require = baseline.require
    rpc = baseline.rpc

    deposit = baseline.receipt(args.l1_rpc_url, args.deposit_tx)
    require(deposit["to"].lower() == args.portal.lower(), "wrong deposit portal")
    block = rpc(args.l1_rpc_url, "eth_getBlockByNumber", deposit["blockNumber"], False)
    require(int(block["timestamp"], 16) >= args.fork_time, "deposit preceded T15")
    tx = rpc(args.l1_rpc_url, "eth_getTransactionByHash", args.deposit_tx)
    require(tx["input"][:10] in DEPOSIT_SELECTORS, "wrong deposit selector")
    require(len(bytes.fromhex(tx["input"][2:])) == 420, "noncanonical deposit size")

    trace = rpc(args.l1_rpc_url, "debug_traceTransaction", args.deposit_tx,
                {"tracer": "callTracer"})
    children = trace.get("calls", [])
    require(trace["to"].lower() == args.portal.lower(), "trace bypassed portal")
    require(all(child["type"] == "CALL" for child in children),
            "proxy delegatecall or unexpected child call")
    require(children and children[0]["to"].lower() == TOKEN
            and children[0]["input"][:10] == TRANSFER_FROM,
            "native portal did not call TIP-20 transferFrom")
    require(all(child["to"].lower() == TOKEN for child in children),
            "native portal called an unexpected dependency")

    settlement = baseline.receipt(args.l1_rpc_url, args.settlement_tx)
    settlement_tx = rpc(args.l1_rpc_url, "eth_getTransactionByHash", args.settlement_tx)
    settlement_call = settlement_tx["calls"][0]
    require(settlement_call["to"].lower() == args.portal.lower(), "wrong settlement portal")
    require(settlement_call["input"][:10] == "0x4cd6c7c7", "wrong settlement selector")
    require(baseline.dynamic_bytes(settlement_call["input"], 11) == b"\x02",
            "expected explicit NoProof development mode")
    require(baseline.dynamic_bytes(settlement_call["input"], 12) == b"",
            "NoProof payload contains nonempty proof")
    settlement_head = bytes.fromhex(settlement_call["input"][10:])
    def word(index):
        return int.from_bytes(settlement_head[index * 32:(index + 1) * 32], "big")
    require(word(0) >= int(deposit["blockNumber"], 16),
            "settlement anchor precedes deposit")
    require((word(6), word(7)) == (args.cursor_before, args.cursor_before + 1),
            "settlement did not advance the expected deposit cursor")

    l1_balance_block = hex(args.l1_balance_block) if args.l1_balance_block else "latest"
    zone_balance_block = hex(args.zone_balance_block) if args.zone_balance_block else "latest"

    def token_balance(url, account, block_id):
        calldata = "0x70a08231" + account.removeprefix("0x").lower().rjust(64, "0")
        return int(rpc(url, "eth_call",
                       {"to": TOKEN, "from": account, "data": calldata}, block_id), 16)

    private_balance = token_balance(args.zone_rpc_url, args.account, zone_balance_block)
    backing = token_balance(args.l1_rpc_url, args.portal, l1_balance_block)
    supply = int(rpc(args.zone_rpc_url, "eth_call",
                     {"to": TOKEN, "data": "0x18160ddd"}, zone_balance_block), 16)
    expected_total = args.expected_total if args.expected_total is not None else args.amount
    require(private_balance == backing == supply == expected_total,
            "private liability and public backing differ")
    require(len([log for log in deposit["logs"]
                 if log["address"].lower() == args.portal.lower()]) == 1,
            "missing unique portal deposit event")

    lane_sample = None
    if args.lane_metrics_file:
        require(block["transactions"] == [args.deposit_tx],
                "lane metric cannot be attributed to a single deposit")
        for line in args.lane_metrics_file.read_text().splitlines():
            candidate = json.loads(line)
            if (int(candidate["time"]) == int(block["timestamp"], 16)
                    and candidate["pool_transactions_included_last"] == 1
                    and candidate["payment_transactions_last"] == 1
                    and candidate["payment_gas_used_last"] == int(deposit["gasUsed"], 16)
                    and candidate["general_gas_used_last"] == 0):
                lane_sample = candidate
                break
        require(lane_sample is not None, "no matching zero-general-gas block metric")

    forged = None
    if args.forged_tx:
        require(args.forged_portal and args.forged_metrics_file,
                "forged transaction requires portal and metric capture")
        forged_receipt = rpc(args.l1_rpc_url, "eth_getTransactionReceipt", args.forged_tx)
        require(forged_receipt is not None and int(forged_receipt["status"], 16) == 0,
                "forged candidate did not revert")
        require(forged_receipt["to"].lower() == args.forged_portal.lower(),
                "forged candidate target mismatch")
        forged_block = rpc(args.l1_rpc_url, "eth_getBlockByNumber",
                           forged_receipt["blockNumber"], False)
        require(forged_block["hash"] == forged_receipt["blockHash"]
                and forged_block["transactions"] == [args.forged_tx],
                "forged candidate block is not canonical and isolated")
        forged_trace = rpc(args.l1_rpc_url, "debug_traceTransaction", args.forged_tx,
                           {"tracer": "callTracer"})
        require(forged_trace.get("error") and not forged_trace.get("calls"),
                "forged candidate entered an external child call")
        require(token_balance(args.l1_rpc_url, args.portal,
                              forged_receipt["blockNumber"]) == expected_total,
                "forged candidate changed portal backing")
        forged_gas = int(forged_receipt["gasUsed"], 16)
        forged_sample = None
        for line in args.forged_metrics_file.read_text().splitlines():
            candidate = json.loads(line)
            if (int(candidate["time"]) == int(forged_block["timestamp"], 16)
                    and candidate["pool_transactions_included_last"] == 1
                    and candidate["payment_transactions_last"] == 0
                    and candidate["payment_gas_used_last"] == 0
                    and candidate["general_gas_used_last"] == forged_gas):
                forged_sample = candidate
                break
        require(forged_sample is not None,
                "forged candidate did not consume general-lane gas")
        forged = {
            "hash": args.forged_tx,
            "portal": args.forged_portal,
            "block": int(forged_receipt["blockNumber"], 16),
            "gas_used": forged_gas,
            "lane_metrics": forged_sample,
        }

    evidence = {
        "kind": "t15_native_portal_deposit_smoke",
        "native_deposit_verified": True,
        "full_native_payment_acceptance": False,
        "proof_mode": "NoProof (0x02)",
        "execution_proof_verified": False,
        "payment_lane_verified": lane_sample is not None,
        "forged_candidate_kept_general": forged is not None,
        "fork_time": args.fork_time,
        "source_revisions": {
            "tempo": args.tempo_revision,
            "zones": args.zones_revision,
        },
        "binary_sha256": {
            "tempo": args.tempo_binary_sha256,
            "zones": args.zone_binary_sha256,
        },
        "l1_client": rpc(args.l1_rpc_url, "web3_clientVersion"),
        "zone_client": rpc(args.zone_rpc_url, "web3_clientVersion"),
        "genesis": {
            "l1": rpc(args.l1_rpc_url, "eth_getBlockByNumber", "0x0", False)["hash"],
            "zone": rpc(args.zone_rpc_url, "eth_getBlockByNumber", "0x0", False)["hash"],
        },
        "portal": args.portal,
        "account": args.account,
        "balances": {
            "l1_block": l1_balance_block,
            "zone_block": zone_balance_block,
            "private": private_balance,
            "portal_backing": backing,
            "zone_supply": supply,
        },
        "deposit": {
            "hash": args.deposit_tx,
            "block": int(deposit["blockNumber"], 16),
            "timestamp": int(block["timestamp"], 16),
            "gas_used": int(deposit["gasUsed"], 16),
            "logs": len(deposit["logs"]),
            "native_child_calls": [
                {"to": child["to"], "selector": child["input"][:10]}
                for child in children
            ],
        },
        "lane_metrics": lane_sample,
        "forged_candidate": forged,
        "settlement": {
            "hash": args.settlement_tx,
            "block": int(settlement["blockNumber"], 16),
            "tempo_anchor_block": word(0),
            "deposit_cursor_before": word(6),
            "deposit_cursor_after": word(7),
            "zone_height": word(13),
            "verifier_config": "0x02",
            "proof_length": 0,
        },
    }
    Path(args.output).write_text(json.dumps(evidence, indent=2) + "\n")
    print(json.dumps({"native_deposit_verified": True, "balances": evidence["balances"]}))


if __name__ == "__main__":
    main()
