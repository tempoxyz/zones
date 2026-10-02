#!/usr/bin/env python3
"""Check a T15 private withdrawal, paid L1 payout, and forged-portal rejection."""

import argparse
import hashlib
import json
from importlib.machinery import SourceFileLoader
from pathlib import Path

baseline = SourceFileLoader(
    "native_payment_baseline", str(Path(__file__).with_name("check-baseline.py"))
).load_module()

PORTAL = "0x5ad0000000000000000000000000000000000003"
FORGED_PORTAL = "0x5ad0000000000000000000000000000000000004"
ACCOUNT = "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"
RECIPIENT_ONE = "0x70997970c51812dc3a010c7d01b50e0d17dc79c8"
RECIPIENT_TWO = "0x3c44cdddb6a900fa2b585dd299e03d12fa4293bc"
WITHDRAWAL_REQUEST = "0x62dfee5e09f557c086bc54e3697269e4542071bd4726d2220e35d58c5ac41f75"
PAYOUT = "0xa4e3fc4cb850cbf71b7604dfcb75680934cf18dab089b6dafacbe511f8735e1b"
FORGED = "0x9efd2115ff508bc896d4fda2eda00e4e0e549d4f77070ee8bed96087241c451e"


def token_balance(url, account, block):
    data = "0x70a08231" + account.removeprefix("0x").rjust(64, "0")
    return int(baseline.rpc(url, "eth_call", {
        "to": baseline.TOKEN, "from": account, "data": data,
    }, hex(block)), 16)


def scalar_call(url, target, signature, block):
    selectors = {"withdrawalQueueHead": "0x94ce88b0",
                 "withdrawalQueueTail": "0x8081a8b2"}
    return int(baseline.rpc(url, "eth_call", {
        "to": target, "data": selectors[signature],
    }, hex(block)), 16)


def matched_metric(path, timestamp, payment_gas, general_gas):
    for line in path.read_text().splitlines():
        sample = json.loads(line)
        if (0 <= sample["time"] - timestamp <= 2
                and sample["pool_transactions_included_last"] == 1
                and sample["payment_gas_used_last"] == payment_gas
                and sample["general_gas_used_last"] == general_gas
                and sample["payment_transactions_last"] == int(payment_gas > 0)):
            return sample
    raise RuntimeError("no matching isolated block lane metric")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--l1-rpc-url", default="http://127.0.0.1:38545")
    parser.add_argument("--zone-rpc-url", default="http://127.0.0.1:49545")
    parser.add_argument("--payout-metrics", type=Path,
                        default=Path("/tmp/evm2-t15-zone3-withdrawal-lane-metrics.jsonl"))
    parser.add_argument("--forged-metrics", type=Path,
                        default=Path("/tmp/evm2-t15-zone3-forged-withdrawal-metrics.jsonl"))
    parser.add_argument("--tempo-binary", type=Path,
                        default=Path("/tmp/native-payments-tempo/target/release/tempo"))
    parser.add_argument("--zone-binary", type=Path, default=Path("target/release/tempo-zone"))
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    rpc = baseline.rpc
    require = baseline.require

    request = baseline.receipt(args.zone_rpc_url, WITHDRAWAL_REQUEST)
    require(int(request["blockNumber"], 16) == 2430,
            "withdrawal request not at expected Zone height")
    balances = {
        "sender": token_balance(args.zone_rpc_url, ACCOUNT, 2430),
        "recipient_one": token_balance(args.zone_rpc_url, RECIPIENT_ONE, 2430),
        "recipient_two": token_balance(args.zone_rpc_url, RECIPIENT_TWO, 2430),
        "zone_supply": int(rpc(args.zone_rpc_url, "eth_call", {
            "to": baseline.TOKEN, "data": "0x18160ddd"}, hex(2430)), 16),
        "backing_before": token_balance(args.l1_rpc_url, PORTAL, 5189),
        "backing_after": token_balance(args.l1_rpc_url, PORTAL, 5190),
        "public_recipient_before": token_balance(args.l1_rpc_url, ACCOUNT, 5189),
        "public_recipient_after": token_balance(args.l1_rpc_url, ACCOUNT, 5190),
    }
    require((balances["sender"], balances["recipient_one"],
             balances["recipient_two"], balances["zone_supply"]) ==
            (500_000, 350_000, 50_000, 900_000), "Zone burn or balances mismatch")
    require((balances["backing_before"], balances["backing_after"]) ==
            (1_000_000, 900_000), "portal backing did not pay out exactly 100,000")
    require(balances["public_recipient_after"] - balances["public_recipient_before"] == 100_000,
            "public recipient did not receive exact payout")

    receipt = baseline.receipt(args.l1_rpc_url, PAYOUT)
    require(int(receipt["blockNumber"], 16) == 5190, "payout in wrong L1 block")
    block = rpc(args.l1_rpc_url, "eth_getBlockByNumber", receipt["blockNumber"], False)
    require(block["transactions"] == [PAYOUT], "payout block is not isolated")
    tx = rpc(args.l1_rpc_url, "eth_getTransactionByHash", PAYOUT)
    require(len(tx.get("calls", [])) == 1
            and tx["calls"][0]["to"].lower() == PORTAL
            and tx["calls"][0]["input"][:10] == "0x91aa3f04",
            "not one canonical portal processWithdrawals AA call")
    trace = rpc(args.l1_rpc_url, "debug_traceTransaction", PAYOUT,
                {"tracer": "callTracer"})
    delegates = [c for c in trace.get("calls", [])
                 if c["type"] == "DELEGATECALL"
                 and c["to"].lower() == "0x5ad1000000000000000000000000000000000000"]
    require(len(delegates) == 1
            and any(c["type"] == "CALL" and c["to"].lower() == baseline.TOKEN
                    and c["input"][:10] == "0xa9059cbb"
                    for c in delegates[0].get("calls", [])),
            "native delegate frame did not pay TIP-20 recipient")
    topic = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"
    transfers = [log for log in receipt["logs"] if log["address"].lower() == baseline.TOKEN
                 and log["topics"][0] == topic
                 and log["topics"][1][-40:] == PORTAL[2:]
                 and log["topics"][2][-40:] == ACCOUNT[2:]
                 and int(log["data"], 16) == 100_000]
    require(len(transfers) == 1, "missing exact portal-to-recipient transfer")
    queue = {
        "head_before": scalar_call(args.l1_rpc_url, PORTAL, "withdrawalQueueHead", 5189),
        "head_after": scalar_call(args.l1_rpc_url, PORTAL, "withdrawalQueueHead", 5190),
        "tail_after": scalar_call(args.l1_rpc_url, PORTAL, "withdrawalQueueTail", 5190),
    }
    require(queue == {"head_before": 1, "head_after": 2, "tail_after": 2},
            "FIFO withdrawal queue did not advance once")
    payout_gas = int(receipt["gasUsed"], 16)
    payout_metric = matched_metric(args.payout_metrics, int(block["timestamp"], 16),
                                   payout_gas, 0)

    forged = rpc(args.l1_rpc_url, "eth_getTransactionReceipt", FORGED)
    require(forged is not None and int(forged["status"], 16) == 0
            and forged["to"].lower() == FORGED_PORTAL,
            "forged portal call did not revert")
    forged_block = rpc(args.l1_rpc_url, "eth_getBlockByNumber", forged["blockNumber"], False)
    require(forged_block["transactions"] == [FORGED], "forged block is not isolated")
    forged_trace = rpc(args.l1_rpc_url, "debug_traceTransaction", FORGED,
                       {"tracer": "callTracer"})
    require(forged_trace.get("error") and not forged_trace.get("calls"),
            "forged candidate entered implementation or child call")
    require(token_balance(args.l1_rpc_url, PORTAL, int(forged["blockNumber"], 16)) == 900_000,
            "forged call changed real portal backing")
    forged_gas = int(forged["gasUsed"], 16)
    forged_metric = matched_metric(args.forged_metrics,
                                   int(forged_block["timestamp"], 16), 0, forged_gas)
    digest = lambda path: hashlib.sha256(path.read_bytes()).hexdigest()
    evidence = {
        "kind": "t15_native_withdrawal_smoke",
        "full_native_payment_acceptance": False,
        "execution_proof_verified": False,
        "proof_mode": "NoProof (0x02)",
        "source_revisions": {"tempo": "e96f3f3bb", "zones": "afbabc891"},
        "binary_sha256": {"tempo": digest(args.tempo_binary),
                          "zones": digest(args.zone_binary)},
        "clients": {"l1": rpc(args.l1_rpc_url, "web3_clientVersion"),
                    "zone": rpc(args.zone_rpc_url, "web3_clientVersion")},
        "genesis": {"l1": rpc(args.l1_rpc_url, "eth_getBlockByNumber", "0x0", False)["hash"],
                    "zone": rpc(args.zone_rpc_url, "eth_getBlockByNumber", "0x0", False)["hash"]},
        "withdrawal_request": {"hash": WITHDRAWAL_REQUEST, "zone_block": 2430,
                               "gas_used": int(request["gasUsed"], 16)},
        "balances": balances,
        "fifo_queue": queue,
        "payout": {"hash": PAYOUT, "l1_block": 5190, "gas_used": payout_gas,
                   "lane_metric": payout_metric,
                   "native_delegate_target": delegates[0]["to"]},
        "forged_candidate": {"hash": FORGED,
                             "l1_block": int(forged["blockNumber"], 16),
                             "gas_used": forged_gas, "lane_metric": forged_metric},
    }
    args.output.write_text(json.dumps(evidence, indent=2) + "\n")
    print(json.dumps({"payout": 100_000, "backing": 900_000,
                      "payout_payment_lane": True, "forged_general_lane": True}))


if __name__ == "__main__":
    main()
