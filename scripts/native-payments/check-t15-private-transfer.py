#!/usr/bin/env python3
"""Verify T15 Zone private transfers, backing, restart state, and L1 batch lane."""

import argparse
import hashlib
import json
from importlib.machinery import SourceFileLoader
from pathlib import Path

baseline = SourceFileLoader(
    "native_payment_baseline", str(Path(__file__).with_name("check-baseline.py"))
).load_module()

PORTAL = "0x5ad0000000000000000000000000000000000003"
SENDER = "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"
FIRST_RECIPIENT = "0x70997970c51812dc3a010c7d01b50e0d17dc79c8"
SECOND_RECIPIENT = "0x3c44cdddb6a900fa2b585dd299e03d12fa4293bc"
BOUNCED_DEPOSIT = "0xe64d25fd96ffb02c75f9f9f7304a3e694f2fd551e5adaa6e4093d1f488d56b3b"
DEPOSIT = "0xd89be43645e2fdd5066ac9fd95bab7e9411e52ffac9f10272d380f23777a9dfe"
TRANSFERS = (
    ("0xc134b827f0a2c49ade98b9cd4f1c8ad7dd58cbef7ee14d89f154e2a1cc839c62", 281, FIRST_RECIPIENT, 250_000),
    ("0x4069cb21771a95ceb4901dbeeaf4236cb3ddbcf1ecf3a7a03e939c700011372b", 350, FIRST_RECIPIENT, 100_000),
    ("0x46649b1ccc33c1d2eeff551f77bb34f78d66a77993e2cf3a324a5609b0a8220e", 448, SECOND_RECIPIENT, 50_000),
)
BATCH = "0x199e855ed2dafb7c72e6dea8a9162ee48530ac1925e60228f14b1c72a9443c8b"


def balance(url, owner, block):
    data = "0x70a08231" + owner.removeprefix("0x").lower().rjust(64, "0")
    return int(baseline.rpc(url, "eth_call", {
        "to": baseline.TOKEN, "from": owner, "data": data,
    }, hex(block)), 16)


def receipt(url, tx_hash, expected_block):
    result = baseline.receipt(url, tx_hash)
    baseline.require(int(result["blockNumber"], 16) == expected_block,
                     f"unexpected block for {tx_hash}")
    return result


def transfer_receipt(url, tx_hash, block, recipient, amount):
    result = receipt(url, tx_hash, block)
    tx = baseline.rpc(url, "eth_getTransactionByHash", tx_hash)
    baseline.require(tx["from"].lower() == SENDER and tx["to"].lower() == baseline.TOKEN,
                     "transfer sender/target mismatch")
    baseline.require(tx["input"][:10] == "0xa9059cbb", "wrong transfer selector")
    topic = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"
    transfers = [log for log in result["logs"] if log["address"].lower() == baseline.TOKEN
                 and log["topics"][0] == topic
                 and log["topics"][1][-40:] == SENDER[2:]
                 and log["topics"][2][-40:] == recipient[2:]]
    baseline.require(len(transfers) == 1 and int(transfers[0]["data"], 16) == amount,
                     "missing exact private transfer event")
    return {"hash": tx_hash, "block": block, "amount": amount,
            "recipient": recipient, "gas_used": int(result["gasUsed"], 16)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--l1-rpc-url", default="http://127.0.0.1:38545")
    parser.add_argument("--zone-rpc-url", default="http://127.0.0.1:49545")
    parser.add_argument("--zone-genesis", type=Path,
                        default=Path("/tmp/evm2-native-payments-t15-transfer-zone/genesis.json"))
    parser.add_argument("--tempo-binary", type=Path,
                        default=Path("/tmp/native-payments-tempo/target/release/tempo"))
    parser.add_argument("--zone-binary", type=Path, default=Path("target/release/tempo-zone"))
    parser.add_argument("--lane-metrics", type=Path,
                        default=Path("/tmp/evm2-t15-transfer-zone3-lane-metrics.jsonl"))
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    rpc = baseline.rpc
    require = baseline.require

    genesis = json.loads(args.zone_genesis.read_text())
    require(genesis["config"]["chainId"] == 5742371274755
            and genesis["config"]["t15Time"] == 1790913144,
            "Zone genesis lacks the scheduled T15 execution fork")
    require(int(rpc(args.zone_rpc_url, "eth_chainId"), 16) == 5742371274755,
            "connected to wrong Zone")
    require(int(rpc(args.zone_rpc_url, "eth_blockNumber"), 16) >= 448,
            "Zone head does not include all transfers")
    bounce = receipt(args.l1_rpc_url, BOUNCED_DEPOSIT, 2669)
    require(balance(args.l1_rpc_url, PORTAL, 2899) == 0,
            "invalid encrypted deposit was not refunded")
    deposit = receipt(args.l1_rpc_url, DEPOSIT, 2901)
    deposit_block = rpc(args.l1_rpc_url, "eth_getBlockByNumber", deposit["blockNumber"], False)
    require(int(deposit_block["timestamp"], 16) >= 1790913144,
            "deposit preceded T15")
    trace = rpc(args.l1_rpc_url, "debug_traceTransaction", DEPOSIT,
                {"tracer": "callTracer"})
    require(trace["to"].lower() == PORTAL
            and any(c["type"] == "CALL" and c["to"].lower() == baseline.TOKEN
                    and c["input"][:10] == "0x23b872dd" for c in trace.get("calls", [])),
            "post-T15 native deposit did not call TIP-20")
    require(balance(args.zone_rpc_url, SENDER, 267) == 0
            and balance(args.zone_rpc_url, SENDER, 268) == 1_000_000,
            "valid deposit did not credit private account exactly once")

    transfers = [transfer_receipt(args.zone_rpc_url, *entry) for entry in TRANSFERS]
    require(transfers[0]["gas_used"] - transfers[1]["gas_used"] == 250_000,
            "unexpected first-transaction gas difference")
    require(abs(transfers[2]["gas_used"] - transfers[1]["gas_used"]) <= 32,
            "new recipient changed steady-state transfer gas")
    require(int(rpc(args.zone_rpc_url, "eth_getTransactionCount", SENDER, hex(280)), 16) == 0
            and int(rpc(args.zone_rpc_url, "eth_getTransactionCount", SENDER, hex(281)), 16) == 1,
            "first-transaction nonce evidence missing")
    balances = {"sender": balance(args.zone_rpc_url, SENDER, 448),
                "recipient_one": balance(args.zone_rpc_url, FIRST_RECIPIENT, 448),
                "recipient_two": balance(args.zone_rpc_url, SECOND_RECIPIENT, 448),
                "portal_backing": balance(args.l1_rpc_url, PORTAL, 3047),
                "zone_supply": int(rpc(args.zone_rpc_url, "eth_call", {
                    "to": baseline.TOKEN, "data": "0x18160ddd"}, hex(448)), 16)}
    require(balances == {"sender": 600_000, "recipient_one": 350_000,
                         "recipient_two": 50_000, "portal_backing": 1_000_000,
                         "zone_supply": 1_000_000}, "private balances and backing diverged")

    batch = receipt(args.l1_rpc_url, BATCH, 3047)
    batch_block = rpc(args.l1_rpc_url, "eth_getBlockByNumber", "0xbe7", False)
    require(batch_block["transactions"] == [BATCH], "batch block is not isolated")
    tx = rpc(args.l1_rpc_url, "eth_getTransactionByHash", BATCH)
    require(len(tx["calls"]) == 1 and tx["calls"][0]["to"].lower() == PORTAL,
            "batch contains unrelated AA calls")
    calldata = tx["calls"][0]["input"]
    require(calldata[:10] == "0x4cd6c7c7"
            and baseline.dynamic_bytes(calldata, 11) == b"\x02"
            and baseline.dynamic_bytes(calldata, 12) == b"",
            "batch did not use explicit NoProof mode")
    head = bytes.fromhex(calldata[10:])
    word = lambda n: int.from_bytes(head[n*32:(n+1)*32], "big")
    require(word(13) == 350 and (word(6), word(7)) == (2, 2),
            "batch does not settle the second transfer window")
    gas = int(batch["gasUsed"], 16)
    timestamp = int(batch_block["timestamp"], 16)
    samples = [json.loads(line) for line in args.lane_metrics.read_text().splitlines()]
    matches = [s for s in samples if 0 <= s["time"] - timestamp <= 2
               and s["pool_transactions_included_last"] == 1
               and s["payment_transactions_last"] == 1
               and s["payment_gas_used_last"] == gas
               and s["general_gas_used_last"] == 0]
    require(matches, "settlement used general-lane capacity")
    digest = lambda path: hashlib.sha256(path.read_bytes()).hexdigest()
    evidence = {
        "kind": "t15_private_transfer_and_payment_settlement_smoke",
        "full_native_payment_acceptance": False,
        "execution_proof_verified": False,
        "zone_t15_scheduled": True,
        "zone_source_revision": "afbabc891",
        "tempo_source_revision": "3ce628b69",
        "binary_sha256": {"tempo": digest(args.tempo_binary),
                          "zones": digest(args.zone_binary)},
        "clients": {"l1": rpc(args.l1_rpc_url, "web3_clientVersion"),
                    "zone": rpc(args.zone_rpc_url, "web3_clientVersion")},
        "genesis": {"l1": rpc(args.l1_rpc_url, "eth_getBlockByNumber", "0x0", False)["hash"],
                    "zone": rpc(args.zone_rpc_url, "eth_getBlockByNumber", "0x0", False)["hash"]},
        "bounced_deposit": {"hash": BOUNCED_DEPOSIT, "block": 2669,
                            "l1_gas_used": int(bounce["gasUsed"], 16),
                            "portal_backing_after_refund": 0},
        "valid_deposit": {"hash": DEPOSIT, "l1_block": 2901,
                          "zone_credit_block": 268, "amount": 1_000_000},
        "transfers": transfers,
        "balances_at_zone_block_448": balances,
        "settled_batch": {"hash": BATCH, "l1_block": 3047,
                          "zone_height": 350, "gas_used": gas,
                          "lane_sample": matches[0], "proof_mode": "NoProof (0x02)"},
    }
    args.output.write_text(json.dumps(evidence, indent=2) + "\n")
    print(json.dumps({"private_transfers": len(transfers), "balances": balances,
                      "batch_payment_lane": True}))


if __name__ == "__main__":
    main()
