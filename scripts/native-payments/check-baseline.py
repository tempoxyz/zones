#!/usr/bin/env python3
"""Verify real baseline bridge receipts and backing; never certify native activation."""
import argparse
import json
from pathlib import Path
import urllib.request

TOKEN = "0x20c0000000000000000000000000000000000000"


def rpc(url, method, *params):
    request = urllib.request.Request(
        url,
        json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode(),
        {"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(request, timeout=10) as response:
        result = json.load(response)
    if "error" in result:
        raise RuntimeError(f"{method}: {result['error']}")
    return result["result"]


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def balance(url, account):
    data = "0x70a08231" + account.removeprefix("0x").lower().rjust(64, "0")
    return int(rpc(url, "eth_call", {"to": TOKEN, "from": account, "data": data}, "latest"), 16)


def receipt(url, transaction_hash):
    result = rpc(url, "eth_getTransactionReceipt", transaction_hash)
    require(result is not None, f"missing receipt: {transaction_hash}")
    require(int(result["status"], 16) == 1, f"reverted: {transaction_hash}")
    block = rpc(url, "eth_getBlockByNumber", result["blockNumber"], False)
    require(block["hash"] == result["blockHash"], f"noncanonical receipt: {transaction_hash}")
    return result


def dynamic_bytes(input_hex, word_index):
    # submitBatch's T13 ABI head is 15 words: two scalars, three static
    # tuples, withdrawal root, config/proof offsets, height, signatures offset.
    data = bytes.fromhex(input_hex.removeprefix("0x"))[4:]
    require(len(data) >= 15 * 32, "truncated settlement calldata")
    offset = int.from_bytes(data[word_index * 32:(word_index + 1) * 32], "big")
    require(offset >= 15 * 32 and offset % 32 == 0, "invalid dynamic ABI offset")
    require(offset + 32 <= len(data), "out-of-range dynamic ABI offset")
    length = int.from_bytes(data[offset:offset + 32], "big")
    require(offset + 32 + length <= len(data), "truncated dynamic ABI bytes")
    return data[offset + 32:offset + 32 + length]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for field in ("l1-rpc-url", "zone-rpc-url", "portal", "account", "public-recipient",
                  "transfer-tx", "deposit-tx", "withdrawal-tx", "settlement-tx", "finalization-tx", "output"):
        parser.add_argument("--" + field, required=True)
    for field in ("zone-balance", "public-recipient-balance", "withdrawal-amount"):
        parser.add_argument("--" + field, required=True, type=int)
    args = parser.parse_args()
    receipts = {
        "transfer": receipt(args.l1_rpc_url, args.transfer_tx),
        "deposit": receipt(args.l1_rpc_url, args.deposit_tx),
        "withdrawal_request": receipt(args.zone_rpc_url, args.withdrawal_tx),
        "settlement": receipt(args.l1_rpc_url, args.settlement_tx),
        "withdrawal_finalization": receipt(args.l1_rpc_url, args.finalization_tx),
    }
    require(receipts["deposit"]["to"].lower() == args.portal.lower(), "wrong deposit portal")
    tx = rpc(args.l1_rpc_url, "eth_getTransactionByHash", args.settlement_tx)
    if "calls" in tx:
        require(len(tx["calls"]) == 1, "expected one settlement call")
        tx = tx["calls"][0]
    require(tx["to"].lower() == args.portal.lower(), "wrong settlement portal")
    require(tx["input"][:10] == "0x4cd6c7c7", "expected T13 settlement selector")
    transfer_topic = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"
    matching = [log for log in receipts["withdrawal_finalization"]["logs"]
                if log["address"].lower() == TOKEN and len(log["topics"]) == 3
                and log["topics"][0] == transfer_topic
                and log["topics"][1][-40:].lower() == args.portal[2:].lower()
                and log["topics"][2][-40:].lower() == args.public_recipient[2:].lower()]
    require(len(matching) == 1, "missing unique portal-to-recipient withdrawal transfer")
    require(int(matching[0]["data"], 16) == args.withdrawal_amount, "wrong withdrawal payout")
    # This artifact deliberately certifies the explicit NoProof baseline only.
    require(dynamic_bytes(tx["input"], 11) == b"\x02", "baseline must use explicit NoProof mode")
    require(dynamic_bytes(tx["input"], 12) == b"", "NoProof baseline must have empty proof")
    balances = {
        "zone_account": balance(args.zone_rpc_url, args.account),
        "portal_backing": balance(args.l1_rpc_url, args.portal),
        "public_recipient": balance(args.l1_rpc_url, args.public_recipient),
        "zone_supply": int(rpc(args.zone_rpc_url, "eth_call", {"to": TOKEN, "data": "0x18160ddd"}, "latest"), 16),
    }
    require(balances["zone_account"] == args.zone_balance, "wrong private account balance")
    require(balances["zone_supply"] == args.zone_balance, "unexpected outstanding private supply")
    require(balances["portal_backing"] == args.zone_balance, "private supply/backing mismatch")
    require(balances["public_recipient"] == args.public_recipient_balance, "wrong public return balance")
    evidence = {
        "kind": "evm2_bridge_baseline", "baseline_checks_pass": True,
        "native_payment_acceptance_pass": False,
        "scheduled_native_upgrade_exercised": False,
        "execution_proof_verified": False, "proof_mode": "NoProof (0x02)",
        "payment_general_lane_attribution": None,
        "clients": {"l1": rpc(args.l1_rpc_url, "web3_clientVersion"),
                    "zone": rpc(args.zone_rpc_url, "web3_clientVersion")},
        "genesis": {"l1": rpc(args.l1_rpc_url, "eth_getBlockByNumber", "0x0", False)["hash"],
                    "zone": rpc(args.zone_rpc_url, "eth_getBlockByNumber", "0x0", False)["hash"]},
        "portal": args.portal, "account": args.account, "public_recipient": args.public_recipient,
        "balances": balances, "receipts": receipts,
    }
    Path(args.output).write_text(json.dumps(evidence, indent=2) + "\n")
    print(json.dumps({"baseline_checks_pass": True, "balances": balances}))


if __name__ == "__main__":
    main()
