#!/usr/bin/env python3
"""Collect receipt-scoped L1 withdrawal capacity measurements."""

from __future__ import annotations

import argparse
import json
import math
import os
import sys
import tempfile
import urllib.error
import urllib.request
from collections import Counter, defaultdict
from pathlib import Path
from typing import Any


WITHDRAWAL_PROCESSED_TOPIC = (
    "0x65042ea6dad60c26f055e80ec401b3437c854ed586a0704d305bb4e9ea4518cf"
)
PROCESS_WITHDRAWALS_SELECTOR = "0x91aa3f04"
WITHDRAWAL_PROCESSED_INPUTS = [
    ("to", "address", True),
    ("senderTag", "bytes32", True),
    ("token", "address", False),
    ("amount", "uint128", False),
    ("callbackSuccess", "bool", False),
]
WITHDRAWAL_CLASSES = ("earn_deposit", "earn_redeem", "offramp")
CALLBACK_CLASSES = ("earn_deposit", "earn_redeem")


class RpcError(RuntimeError):
    """A JSON-RPC request or response was invalid."""


class Rpc:
    def __init__(self, url: str, timeout: float) -> None:
        self.url = url
        self.timeout = timeout
        self.request_id = 0

    def call(self, method: str, params: list[Any]) -> Any:
        self.request_id += 1
        payload = json.dumps(
            {
                "jsonrpc": "2.0",
                "id": self.request_id,
                "method": method,
                "params": params,
            }
        ).encode()
        request = urllib.request.Request(
            self.url,
            data=payload,
            headers={"Content-Type": "application/json"},
            method="POST",
        )
        try:
            with urllib.request.urlopen(request, timeout=self.timeout) as response:
                result = json.load(response)
        except urllib.error.HTTPError as error:
            raise RpcError(f"{method} returned HTTP status {error.code}") from None
        except OSError as error:
            raise RpcError(f"{method} request failed ({type(error).__name__})") from None
        except json.JSONDecodeError:
            raise RpcError(f"{method} returned invalid JSON") from None
        if not isinstance(result, dict):
            raise RpcError(f"{method} returned a non-object response")
        if "error" in result:
            raise RpcError(f"{method} returned an error: {result['error']}")
        if "result" not in result:
            raise RpcError(f"{method} response omitted result")
        return result["result"]


def hex_quantity(value: int) -> str:
    return hex(value)


def parse_quantity(value: Any, field: str) -> int:
    if not isinstance(value, str) or not value.startswith("0x"):
        raise RpcError(f"{field} is not a hexadecimal quantity")
    try:
        return int(value, 16)
    except ValueError:
        raise RpcError(f"{field} is not a hexadecimal quantity") from None


def normalized_hex(value: Any, field: str, size: int | None = None) -> str:
    if not isinstance(value, str) or not value.startswith("0x"):
        raise RpcError(f"{field} is not hexadecimal")
    normalized = value.lower()
    if size is not None and len(normalized) != 2 + size * 2:
        raise RpcError(f"{field} has an unexpected length")
    try:
        int(normalized[2:] or "0", 16)
    except ValueError:
        raise RpcError(f"{field} is not hexadecimal") from None
    return normalized


def percentile(values: list[int], fraction: float) -> int:
    """Return a nearest-rank percentile over a non-empty integer sample."""
    if not values:
        return 0
    ordered = sorted(values)
    return ordered[max(0, math.ceil(fraction * len(ordered)) - 1)]


def distribution(values: list[int]) -> dict[str, int]:
    return {
        "max": max(values, default=0),
        "p50": percentile(values, 0.50),
        "p95": percentile(values, 0.95),
    }


def validate_portal_abi(path: Path) -> str:
    try:
        abi = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise RpcError(f"could not read ZonePortal ABI: {error}") from None
    if not isinstance(abi, list):
        raise RpcError("ZonePortal ABI is not an array")
    events = [
        entry
        for entry in abi
        if isinstance(entry, dict)
        and entry.get("type") == "event"
        and entry.get("name") == "WithdrawalProcessed"
    ]
    if len(events) != 1:
        raise RpcError("ZonePortal ABI must contain one WithdrawalProcessed event")
    event = events[0]
    inputs = event.get("inputs")
    observed_inputs = (
        [
            (entry.get("name"), entry.get("type"), entry.get("indexed"))
            for entry in inputs
            if isinstance(entry, dict)
        ]
        if isinstance(inputs, list)
        else []
    )
    if event.get("anonymous") is not False or observed_inputs != WITHDRAWAL_PROCESSED_INPUTS:
        raise RpcError("ZonePortal WithdrawalProcessed ABI does not match the collector")
    input_types = ",".join(
        field_type for _, field_type, _ in WITHDRAWAL_PROCESSED_INPUTS
    )
    return f"WithdrawalProcessed({input_types})"


def event_key(log: dict[str, Any]) -> tuple[str, int]:
    tx_hash = normalized_hex(log.get("transactionHash"), "log.transactionHash", 32)
    log_index = parse_quantity(log.get("logIndex"), "log.logIndex")
    return tx_hash, log_index


def topic_address(value: Any, field: str) -> str:
    topic = normalized_hex(value, field, 32)
    if topic[2:26] != "0" * 24:
        raise RpcError(f"{field} is not an ABI-encoded address")
    return "0x" + topic[-40:]


def decode_withdrawal_processed(log: dict[str, Any]) -> dict[str, Any]:
    topics = log.get("topics")
    if not isinstance(topics, list) or len(topics) != 3:
        raise RpcError("WithdrawalProcessed log must contain exactly three topics")
    if (
        normalized_hex(topics[0], "WithdrawalProcessed topic0", 32)
        != WITHDRAWAL_PROCESSED_TOPIC
    ):
        raise RpcError("WithdrawalProcessed log has an unexpected topic0")

    recipient = topic_address(topics[1], "WithdrawalProcessed to")
    sender_tag = normalized_hex(topics[2], "WithdrawalProcessed senderTag", 32)
    data = normalized_hex(log.get("data"), "WithdrawalProcessed data", 96)
    words = [data[2 + offset : 2 + offset + 64] for offset in range(0, 192, 64)]
    if words[0][:24] != "0" * 24:
        raise RpcError("WithdrawalProcessed token is not an ABI-encoded address")
    token = "0x" + words[0][-40:]
    amount = int(words[1], 16)
    if amount > 2**128 - 1:
        raise RpcError("WithdrawalProcessed amount exceeds uint128")
    callback_success_value = int(words[2], 16)
    if callback_success_value not in (0, 1):
        raise RpcError("WithdrawalProcessed callbackSuccess is not a boolean")
    return {
        "to": recipient,
        "sender_tag": sender_tag,
        "token": token,
        "amount": amount,
        "callback_success": callback_success_value == 1,
    }


def classify_withdrawal(
    event: dict[str, Any],
    earn_router: str,
    bridge: str,
    stable_tokens: set[str],
    earn_share_token: str,
) -> str | None:
    recipient = event["to"]
    token = event["token"]
    if recipient == earn_router and token in stable_tokens:
        return "earn_deposit"
    if recipient == earn_router and token == earn_share_token:
        return "earn_redeem"
    if recipient == bridge and token in stable_tokens:
        return "offramp"
    return None


def matching_receipt_events(
    receipt: dict[str, Any], portal: str
) -> dict[tuple[str, int], dict[str, Any]]:
    matching: dict[tuple[str, int], dict[str, Any]] = {}
    for log in receipt.get("logs", []):
        if not isinstance(log, dict):
            raise RpcError("receipt.logs contains a non-object")
        address = normalized_hex(log.get("address"), "receipt log address", 20)
        topics = log.get("topics")
        if (
            address != portal
            or not isinstance(topics, list)
            or not topics
            or normalized_hex(topics[0], "receipt log topic", 32)
            != WITHDRAWAL_PROCESSED_TOPIC
        ):
            continue
        key = event_key(log)
        if key in matching:
            raise RpcError(f"receipt contains duplicate WithdrawalProcessed log {key}")
        matching[key] = log
    return matching


def fetch_logs(
    rpc: Rpc, portal: str, from_block: int, to_block: int, max_block_range: int
) -> list[dict[str, Any]]:
    logs: dict[tuple[str, int], dict[str, Any]] = {}
    chunk_start = from_block
    while chunk_start <= to_block:
        chunk_end = min(to_block, chunk_start + max_block_range - 1)
        result = rpc.call(
            "eth_getLogs",
            [
                {
                    "fromBlock": hex_quantity(chunk_start),
                    "toBlock": hex_quantity(chunk_end),
                    "address": portal,
                    "topics": [WITHDRAWAL_PROCESSED_TOPIC],
                }
            ],
        )
        if not isinstance(result, list):
            raise RpcError("eth_getLogs returned a non-array result")
        for log in result:
            if not isinstance(log, dict):
                raise RpcError("eth_getLogs returned a non-object log")
            if log.get("removed") is True:
                raise RpcError("eth_getLogs returned a removed WithdrawalProcessed log")
            address = normalized_hex(log.get("address"), "log.address", 20)
            topics = log.get("topics")
            if address != portal:
                raise RpcError("eth_getLogs returned a log from another address")
            if (
                not isinstance(topics, list)
                or not topics
                or normalized_hex(topics[0], "log.topic0", 32)
                != WITHDRAWAL_PROCESSED_TOPIC
            ):
                raise RpcError("eth_getLogs returned a different event")
            key = event_key(log)
            if key in logs:
                raise RpcError(f"eth_getLogs returned duplicate log {key}")
            logs[key] = log
        chunk_start = chunk_end + 1
    return list(logs.values())


def collect(args: argparse.Namespace) -> dict[str, Any]:
    rpc = Rpc(args.rpc_url, args.rpc_timeout)
    portal = normalized_hex(args.portal, "portal", 20)
    event_signature = validate_portal_abi(args.portal_abi)
    classification_enabled = args.earn_router is not None
    earn_router = (
        normalized_hex(args.earn_router, "earn router", 20)
        if classification_enabled
        else None
    )
    bridge = (
        normalized_hex(args.bridge, "bridge", 20) if classification_enabled else None
    )
    stable_tokens = (
        {
            normalized_hex(token, "stable token", 20)
            for token in args.stable_token
        }
        if classification_enabled
        else set()
    )
    earn_share_token = (
        normalized_hex(args.earn_share_token, "EarnShare token", 20)
        if classification_enabled
        else None
    )
    if classification_enabled:
        if earn_router == bridge:
            raise RpcError("EarnRouter and Bridge destinations must differ")
        if earn_share_token in stable_tokens:
            raise RpcError("EarnShare token must differ from stable tokens")

    expected_class_counts = (
        {
            "earn_deposit": args.expected_earn_deposits,
            "earn_redeem": args.expected_earn_redeems,
            "offramp": args.expected_offramps,
        }
        if args.expected_earn_deposits is not None
        else None
    )
    expected_callback_successes = (
        {
            "earn_deposit": args.expected_earn_deposit_callback_successes,
            "earn_redeem": args.expected_earn_redeem_callback_successes,
        }
        if args.expected_earn_deposit_callback_successes is not None
        else None
    )
    if (
        expected_class_counts is not None
        and sum(expected_class_counts.values()) != args.expected_withdrawals
    ):
        raise RpcError(
            "expected per-class withdrawal counts do not sum to "
            f"--expected-withdrawals ({args.expected_withdrawals})"
        )

    logs = fetch_logs(
        rpc, portal, args.from_block, args.to_block, args.max_block_range
    )
    if len(logs) != args.expected_withdrawals:
        raise RpcError(
            "WithdrawalProcessed count "
            f"{len(logs)} does not equal expected {args.expected_withdrawals}"
        )

    event_classes: dict[tuple[str, int], str] = {}
    observed_class_counts: Counter[str] = Counter()
    observed_callback_successes: Counter[str] = Counter()
    for log in logs:
        key = event_key(log)
        decoded = decode_withdrawal_processed(log)
        if not classification_enabled:
            continue
        assert earn_router is not None
        assert bridge is not None
        assert earn_share_token is not None
        withdrawal_class = classify_withdrawal(
            decoded, earn_router, bridge, stable_tokens, earn_share_token
        )
        if withdrawal_class is None:
            if expected_class_counts is not None:
                raise RpcError(
                    "unexpected WithdrawalProcessed route "
                    f"in transaction {key[0]} log {key[1]}: "
                    f"to={decoded['to']} token={decoded['token']}"
                )
            withdrawal_class = "unexpected"
        event_classes[key] = withdrawal_class
        observed_class_counts[withdrawal_class] += 1
        if withdrawal_class in CALLBACK_CLASSES and decoded["callback_success"]:
            observed_callback_successes[withdrawal_class] += 1

    if expected_class_counts is not None:
        for withdrawal_class in WITHDRAWAL_CLASSES:
            observed = observed_class_counts[withdrawal_class]
            expected = expected_class_counts[withdrawal_class]
            if observed != expected:
                raise RpcError(
                    f"{withdrawal_class} WithdrawalProcessed count {observed} "
                    f"does not equal expected {expected}"
                )
    if expected_callback_successes is not None:
        assert expected_class_counts is not None
        for withdrawal_class in CALLBACK_CLASSES:
            observed_successes = observed_callback_successes[withdrawal_class]
            observed_failures = (
                observed_class_counts[withdrawal_class] - observed_successes
            )
            expected_successes = expected_callback_successes[withdrawal_class]
            expected_failures = (
                expected_class_counts[withdrawal_class] - expected_successes
            )
            if (
                observed_successes != expected_successes
                or observed_failures != expected_failures
            ):
                raise RpcError(
                    f"{withdrawal_class} callback outcomes success/failure "
                    f"{observed_successes}/{observed_failures} do not equal "
                    f"expected {expected_successes}/{expected_failures}"
                )

    queried_events_by_tx: dict[str, dict[tuple[str, int], dict[str, Any]]] = defaultdict(
        dict
    )
    for log in logs:
        key = event_key(log)
        queried_events_by_tx[key[0]][key] = log

    tx_data: dict[str, dict[str, Any]] = {}
    block_to_txs: dict[int, list[str]] = defaultdict(list)
    for tx_hash, queried_events in queried_events_by_tx.items():
        receipt = rpc.call("eth_getTransactionReceipt", [tx_hash])
        if not isinstance(receipt, dict):
            raise RpcError(f"missing receipt for withdrawal transaction {tx_hash}")
        receipt_hash = normalized_hex(
            receipt.get("transactionHash"), "receipt.transactionHash", 32
        )
        if receipt_hash != tx_hash:
            raise RpcError(f"receipt transaction hash does not match {tx_hash}")
        if parse_quantity(receipt.get("status"), "receipt.status") != 1:
            raise RpcError(f"withdrawal transaction {tx_hash} did not succeed")
        block_number = parse_quantity(receipt.get("blockNumber"), "receipt.blockNumber")
        if not args.from_block <= block_number <= args.to_block:
            raise RpcError(f"withdrawal receipt {tx_hash} falls outside measured range")
        block_hash = normalized_hex(receipt.get("blockHash"), "receipt.blockHash", 32)
        gas_used = parse_quantity(receipt.get("gasUsed"), "receipt.gasUsed")

        receipt_events = matching_receipt_events(receipt, portal)
        if set(receipt_events) != set(queried_events):
            raise RpcError(
                f"receipt-scoped WithdrawalProcessed logs do not match eth_getLogs for {tx_hash}"
            )
        for key, queried_log in queried_events.items():
            receipt_log = receipt_events[key]
            for field in (
                "address",
                "blockHash",
                "blockNumber",
                "data",
                "topics",
                "transactionHash",
            ):
                queried_value = queried_log.get(field)
                receipt_value = receipt_log.get(field)
                if isinstance(queried_value, str) and isinstance(receipt_value, str):
                    equal = queried_value.lower() == receipt_value.lower()
                else:
                    equal = queried_value == receipt_value
                if not equal:
                    raise RpcError(f"receipt log {key} differs in {field}")
            log_block = parse_quantity(queried_log.get("blockNumber"), "log.blockNumber")
            log_block_hash = normalized_hex(
                queried_log.get("blockHash"), "log.blockHash", 32
            )
            if log_block != block_number or log_block_hash != block_hash:
                raise RpcError(f"receipt and log block identity differ for {key}")

        transaction = rpc.call("eth_getTransactionByHash", [tx_hash])
        if not isinstance(transaction, dict):
            raise RpcError(f"missing transaction body for {tx_hash}")
        transaction_to = normalized_hex(transaction.get("to"), "transaction.to", 20)
        transaction_input = normalized_hex(
            transaction.get("input"), "transaction.input"
        )
        transaction_gas_limit = parse_quantity(
            transaction.get("gas"), "transaction.gas"
        )
        if transaction_to != portal:
            raise RpcError(f"WithdrawalProcessed transaction {tx_hash} did not call portal")
        if not transaction_input.startswith(PROCESS_WITHDRAWALS_SELECTOR):
            raise RpcError(f"transaction {tx_hash} is not processWithdrawals")

        tx_data[tx_hash] = {
            "block_number": block_number,
            "block_hash": block_hash,
            "gas_used": gas_used,
            "gas_limit": transaction_gas_limit,
            "withdrawal_count": len(queried_events),
        }
        if classification_enabled:
            tx_data[tx_hash]["class_counts"] = dict(
                Counter(event_classes[key] for key in queried_events)
            )
        block_to_txs[block_number].append(tx_hash)

    rows: list[dict[str, Any]] = []
    for block_number in sorted(block_to_txs):
        block = rpc.call(
            "eth_getBlockByNumber", [hex_quantity(block_number), False]
        )
        if not isinstance(block, dict):
            raise RpcError(f"missing L1 block {block_number}")
        observed_number = parse_quantity(block.get("number"), "block.number")
        if observed_number != block_number:
            raise RpcError(f"eth_getBlockByNumber returned block {observed_number}")
        block_hash = normalized_hex(block.get("hash"), "block.hash", 32)
        gas_used = parse_quantity(block.get("gasUsed"), "block.gasUsed")
        gas_limit = parse_quantity(block.get("gasLimit"), "block.gasLimit")
        transactions = block.get("transactions")
        if not isinstance(transactions, list):
            raise RpcError("block.transactions is not an array")
        transaction_hashes = [
            normalized_hex(tx, "block transaction hash", 32) for tx in transactions
        ]
        process_txs = sorted(block_to_txs[block_number])
        for tx_hash in process_txs:
            if tx_data[tx_hash]["block_hash"] != block_hash:
                raise RpcError(f"block hash does not match receipt for {tx_hash}")
            if tx_hash not in transaction_hashes:
                raise RpcError(f"L1 block {block_number} omits process transaction {tx_hash}")
        row = {
            "block_number": block_number,
            "block_hash": block_hash,
            "withdrawal_count": sum(
                tx_data[tx_hash]["withdrawal_count"] for tx_hash in process_txs
            ),
            "process_tx_count": len(process_txs),
            "process_tx_gas_used": sum(
                tx_data[tx_hash]["gas_used"] for tx_hash in process_txs
            ),
            "process_tx_gas_limit": sum(
                tx_data[tx_hash]["gas_limit"] for tx_hash in process_txs
            ),
            "l1_block_gas_used": gas_used,
            "l1_block_gas_limit": gas_limit,
            "l1_block_gas_utilization_bps": (
                gas_used * 10_000 // gas_limit if gas_limit else 0
            ),
            "l1_tx_count": len(transaction_hashes),
            "process_tx_hashes": process_txs,
        }
        if classification_enabled:
            row["class_counts"] = {
                withdrawal_class: sum(
                    tx_data[tx_hash]["class_counts"].get(withdrawal_class, 0)
                    for tx_hash in process_txs
                )
                for withdrawal_class in (
                    *WITHDRAWAL_CLASSES,
                    *(("unexpected",) if observed_class_counts["unexpected"] else ()),
                )
            }
        rows.append(row)

    process_gas_samples = [entry["gas_used"] for entry in tx_data.values()]
    process_gas_limit_samples = [entry["gas_limit"] for entry in tx_data.values()]
    measured_block_count = args.to_block - args.from_block + 1
    empty_block_count = measured_block_count - len(rows)
    withdrawals_per_measured_block = [
        row["withdrawal_count"] for row in rows
    ] + [0] * empty_block_count
    process_txs_per_measured_block = [
        row["process_tx_count"] for row in rows
    ] + [0] * empty_block_count
    first_event_block = rows[0]["block_number"] if rows else None
    last_event_block = rows[-1]["block_number"] if rows else None
    event_span_block_count = (
        last_event_block - first_event_block + 1
        if first_event_block is not None and last_event_block is not None
        else 0
    )
    withdrawals_by_block = {row["block_number"]: row["withdrawal_count"] for row in rows}
    withdrawals_per_event_span_block = (
        [
            withdrawals_by_block.get(block_number, 0)
            for block_number in range(first_event_block, last_event_block + 1)
        ]
        if first_event_block is not None and last_event_block is not None
        else []
    )
    report = {
        "schema_version": 3,
        "portal": portal,
        "event": event_signature,
        "process_function": (
            "processWithdrawals("
            "(address,bytes32,address,uint128,bytes32,uint64,uint64,bytes,bytes)[],"
            "bytes32)"
        ),
        "measured_l1_range": {
            "start_block": args.from_block,
            "end_block": args.to_block,
            "block_count": measured_block_count,
        },
        "withdrawal_event_span": {
            "start_block": first_event_block,
            "end_block": last_event_block,
            "block_count": event_span_block_count,
        },
        "totals": {
            "withdrawal_count": len(logs),
            "process_tx_count": len(tx_data),
            "process_tx_gas_used": sum(process_gas_samples),
            "process_tx_gas_limit": sum(process_gas_limit_samples),
            "blocks_with_withdrawals": len(rows),
        },
        "distribution": {
            "withdrawals_per_active_block": distribution(
                [row["withdrawal_count"] for row in rows]
            ),
            "process_txs_per_active_block": distribution(
                [row["process_tx_count"] for row in rows]
            ),
            "withdrawals_per_measured_block": distribution(
                withdrawals_per_measured_block
            ),
            "withdrawals_per_event_span_block": distribution(
                withdrawals_per_event_span_block
            ),
            "process_txs_per_measured_block": distribution(
                process_txs_per_measured_block
            ),
            "process_tx_gas_used_per_active_block": distribution(
                [row["process_tx_gas_used"] for row in rows]
            ),
            "process_tx_gas_limit_per_active_block": distribution(
                [row["process_tx_gas_limit"] for row in rows]
            ),
            "process_tx_gas_used": distribution(process_gas_samples),
            "process_tx_gas_limit": distribution(process_gas_limit_samples),
            "l1_block_gas_used": distribution(
                [row["l1_block_gas_used"] for row in rows]
            ),
            "l1_block_gas_utilization_bps": distribution(
                [row["l1_block_gas_utilization_bps"] for row in rows]
            ),
        },
        "rows": rows,
    }
    if classification_enabled:
        classification_labels = [
            *WITHDRAWAL_CLASSES,
            *(["unexpected"] if observed_class_counts["unexpected"] else []),
        ]
        class_reports: dict[str, Any] = {}
        for withdrawal_class in classification_labels:
            class_rows = [
                row
                for row in rows
                if row["class_counts"].get(withdrawal_class, 0) > 0
            ]
            counts_by_block = {
                row["block_number"]: row["class_counts"].get(withdrawal_class, 0)
                for row in rows
            }
            active_counts = [
                row["class_counts"][withdrawal_class] for row in class_rows
            ]
            measured_counts = [
                counts_by_block.get(block_number, 0)
                for block_number in range(args.from_block, args.to_block + 1)
            ]
            first_class_block = (
                class_rows[0]["block_number"] if class_rows else None
            )
            last_class_block = (
                class_rows[-1]["block_number"] if class_rows else None
            )
            class_span_count = (
                last_class_block - first_class_block + 1
                if first_class_block is not None and last_class_block is not None
                else 0
            )
            span_counts = (
                [
                    counts_by_block.get(block_number, 0)
                    for block_number in range(first_class_block, last_class_block + 1)
                ]
                if first_class_block is not None and last_class_block is not None
                else []
            )
            busiest = sorted(
                (
                    {
                        "block_number": row["block_number"],
                        "withdrawal_count": row["class_counts"][withdrawal_class],
                        "process_tx_count": row["process_tx_count"],
                        "process_tx_gas_used": row["process_tx_gas_used"],
                        "l1_block_gas_used": row["l1_block_gas_used"],
                        "l1_block_gas_limit": row["l1_block_gas_limit"],
                    }
                    for row in class_rows
                ),
                key=lambda row: (-row["withdrawal_count"], row["block_number"]),
            )[:10]
            class_report = {
                "withdrawal_count": observed_class_counts[withdrawal_class],
                "blocks_with_withdrawals": len(class_rows),
                "withdrawal_event_span": {
                    "start_block": first_class_block,
                    "end_block": last_class_block,
                    "block_count": class_span_count,
                },
                "distribution": {
                    "withdrawals_per_active_block": distribution(active_counts),
                    "withdrawals_per_measured_block": distribution(measured_counts),
                    "withdrawals_per_event_span_block": distribution(span_counts),
                },
                "busiest_blocks": busiest,
            }
            if withdrawal_class in CALLBACK_CLASSES:
                success_count = observed_callback_successes[withdrawal_class]
                class_report["callback_outcomes"] = {
                    "applicable": True,
                    "success_count": success_count,
                    "failure_count": (
                        observed_class_counts[withdrawal_class] - success_count
                    ),
                }
                if expected_callback_successes is not None:
                    assert expected_class_counts is not None
                    expected_success_count = expected_callback_successes[
                        withdrawal_class
                    ]
                    class_report["callback_outcomes"].update(
                        {
                            "expected_success_count": expected_success_count,
                            "expected_failure_count": (
                                expected_class_counts[withdrawal_class]
                                - expected_success_count
                            ),
                        }
                    )
            else:
                # WithdrawalProcessed.callbackSuccess is not a business outcome
                # when requestWithdrawal supplied no callback.
                class_report["callback_outcomes"] = {"applicable": False}
            if (
                expected_class_counts is not None
                and withdrawal_class in expected_class_counts
            ):
                class_report["expected_withdrawal_count"] = expected_class_counts[
                    withdrawal_class
                ]
            class_reports[withdrawal_class] = class_report

        report["withdrawal_classes"] = {
            "routes": {
                "earn_router": earn_router,
                "bridge": bridge,
                "stable_tokens": sorted(stable_tokens),
                "earn_share_token": earn_share_token,
            },
            "classes": class_reports,
        }
    return report


def write_atomic(path: Path, contents: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary_name = tempfile.mkstemp(
        dir=path.parent, prefix=f".{path.name}.", text=True
    )
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8") as handle:
            handle.write(contents)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temporary_name, path)
    except BaseException:
        try:
            os.unlink(temporary_name)
        except FileNotFoundError:
            pass
        raise


def render_markdown(report: dict[str, Any]) -> str:
    measured_range = report["measured_l1_range"]
    event_span = report["withdrawal_event_span"]
    totals = report["totals"]
    distributions = report["distribution"]
    withdrawals = distributions["withdrawals_per_active_block"]
    measured_withdrawals = distributions["withdrawals_per_measured_block"]
    span_withdrawals = distributions["withdrawals_per_event_span_block"]
    process_gas = distributions["process_tx_gas_used"]
    process_gas_limit = distributions["process_tx_gas_limit"]
    rows = sorted(
        report["rows"],
        key=lambda row: (-row["withdrawal_count"], row["block_number"]),
    )[:10]
    lines = [
        "## L1 withdrawal block capacity",
        "",
        (
            f"- Measured L1 range: `{measured_range['start_block']}`–"
            f"`{measured_range['end_block']}` "
            f"({measured_range['block_count']} blocks)"
        ),
        (
            f"- Exact `WithdrawalProcessed` events: `{totals['withdrawal_count']}` "
            f"across `{totals['blocks_with_withdrawals']}` active blocks"
        ),
        (
            f"- `processWithdrawals` transactions: `{totals['process_tx_count']}`; "
            f"total gas used / declared: `{totals['process_tx_gas_used']}` / "
            f"`{totals['process_tx_gas_limit']}`"
        ),
        (
            "- Withdrawals per active block (p50 / p95 / max): "
            f"`{withdrawals['p50']}` / `{withdrawals['p95']}` / "
            f"`{withdrawals['max']}`"
        ),
        (
            "- Withdrawals per measured block, including zeroes (p50 / p95 / max): "
            f"`{measured_withdrawals['p50']}` / `{measured_withdrawals['p95']}` / "
            f"`{measured_withdrawals['max']}`"
        ),
        (
            f"- Withdrawal event span: `{event_span['start_block']}`–"
            f"`{event_span['end_block']}` ({event_span['block_count']} blocks); "
            "withdrawals per span block including internal zeroes "
            f"(p50 / p95 / max): `{span_withdrawals['p50']}` / "
            f"`{span_withdrawals['p95']}` / `{span_withdrawals['max']}`"
        ),
        (
            "- Process-transaction gas (p50 / p95 / max): "
            f"`{process_gas['p50']}` / `{process_gas['p95']}` / "
            f"`{process_gas['max']}`"
        ),
        (
            "- Process-transaction declared gas (p50 / p95 / max): "
            f"`{process_gas_limit['p50']}` / `{process_gas_limit['p95']}` / "
            f"`{process_gas_limit['max']}`"
        ),
        "",
    ]
    withdrawal_classes = report.get("withdrawal_classes")
    if isinstance(withdrawal_classes, dict):
        classes = withdrawal_classes["classes"]
        lines.extend(
            [
                "### Withdrawal classes",
                "",
                (
                    "| Class | Events | Callback success / failure | Active blocks | "
                    "Per active block p50 / p95 / max | "
                    "Per measured block p50 / p95 / max |"
                ),
                "| --- | ---: | ---: | ---: | ---: | ---: |",
            ]
        )
        for withdrawal_class, class_report in classes.items():
            active = class_report["distribution"]["withdrawals_per_active_block"]
            measured = class_report["distribution"][
                "withdrawals_per_measured_block"
            ]
            display_name = withdrawal_class.replace("_", " ")
            callback_outcomes = class_report["callback_outcomes"]
            callback_display = (
                f"{callback_outcomes['success_count']} / "
                f"{callback_outcomes['failure_count']}"
                if callback_outcomes["applicable"]
                else "n/a (no callback)"
            )
            lines.append(
                f"| {display_name} | {class_report['withdrawal_count']} | "
                f"{callback_display} | "
                f"{class_report['blocks_with_withdrawals']} | "
                f"{active['p50']} / {active['p95']} / {active['max']} | "
                f"{measured['p50']} / {measured['p95']} / "
                f"{measured['max']} |"
            )
        lines.append("")

        busiest_class_rows = [
            (withdrawal_class, row)
            for withdrawal_class, class_report in classes.items()
            for row in class_report["busiest_blocks"]
        ]
        if busiest_class_rows:
            lines.extend(
                [
                    "Busiest blocks by withdrawal class (up to 10 per class):",
                    "",
                    (
                        "| Class | L1 block | Withdrawals | Process txs | "
                        "Process gas used | Block gas | Gas limit |"
                    ),
                    "| --- | ---: | ---: | ---: | ---: | ---: | ---: |",
                ]
            )
            for withdrawal_class, row in busiest_class_rows:
                lines.append(
                    f"| {withdrawal_class.replace('_', ' ')} | "
                    f"{row['block_number']} | {row['withdrawal_count']} | "
                    f"{row['process_tx_count']} | {row['process_tx_gas_used']} | "
                    f"{row['l1_block_gas_used']} | {row['l1_block_gas_limit']} |"
                )
            lines.append("")

    if rows:
        lines.extend(
            [
                "Busiest withdrawal blocks (up to 10):",
                "",
                (
                    "| L1 block | Withdrawals | Process txs | Process gas used | "
                    "Process gas declared | Block gas | Gas limit | L1 txs |"
                ),
                "| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
            ]
        )
        for row in rows:
            lines.append(
                f"| {row['block_number']} | {row['withdrawal_count']} | "
                f"{row['process_tx_count']} | {row['process_tx_gas_used']} | "
                f"{row['process_tx_gas_limit']} | "
                f"{row['l1_block_gas_used']} | {row['l1_block_gas_limit']} | "
                f"{row['l1_tx_count']} |"
            )
        lines.append("")
    return "\n".join(lines)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--rpc-url", required=True)
    parser.add_argument("--portal", required=True)
    parser.add_argument("--portal-abi", type=Path, required=True)
    parser.add_argument("--from-block", type=int, required=True)
    parser.add_argument("--to-block", type=int, required=True)
    parser.add_argument("--expected-withdrawals", type=int, required=True)
    parser.add_argument("--earn-router")
    parser.add_argument("--bridge")
    parser.add_argument("--stable-token", action="append", default=[])
    parser.add_argument("--earn-share-token")
    parser.add_argument("--expected-earn-deposits", type=int)
    parser.add_argument("--expected-earn-redeems", type=int)
    parser.add_argument("--expected-offramps", type=int)
    parser.add_argument("--expected-earn-deposit-callback-successes", type=int)
    parser.add_argument("--expected-earn-redeem-callback-successes", type=int)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--markdown-output", type=Path)
    parser.add_argument("--max-block-range", type=int, default=1000)
    parser.add_argument("--rpc-timeout", type=float, default=10.0)
    args = parser.parse_args()
    if args.from_block < 0 or args.to_block < args.from_block:
        parser.error("invalid measured L1 block range")
    if args.expected_withdrawals < 0:
        parser.error("expected withdrawals must be non-negative")
    route_arguments = (
        args.earn_router is not None,
        args.bridge is not None,
        bool(args.stable_token),
        args.earn_share_token is not None,
    )
    if any(route_arguments) and not all(route_arguments):
        parser.error(
            "--earn-router, --bridge, at least one --stable-token, and "
            "--earn-share-token must be supplied together"
        )
    expected_class_counts = (
        args.expected_earn_deposits,
        args.expected_earn_redeems,
        args.expected_offramps,
    )
    if any(value is not None for value in expected_class_counts):
        if not all(value is not None for value in expected_class_counts):
            parser.error(
                "--expected-earn-deposits, --expected-earn-redeems, and "
                "--expected-offramps must be supplied together"
            )
        if not all(route_arguments):
            parser.error(
                "per-class expectations require withdrawal route addresses"
            )
        if any(value < 0 for value in expected_class_counts):
            parser.error("expected per-class withdrawals must be non-negative")
    expected_callback_successes = (
        args.expected_earn_deposit_callback_successes,
        args.expected_earn_redeem_callback_successes,
    )
    if any(value is not None for value in expected_callback_successes):
        if not all(value is not None for value in expected_callback_successes):
            parser.error(
                "--expected-earn-deposit-callback-successes and "
                "--expected-earn-redeem-callback-successes must be supplied together"
            )
        if args.expected_earn_deposits is None:
            parser.error(
                "callback expectations require per-class withdrawal expectations"
            )
        callback_limits = (
            args.expected_earn_deposits,
            args.expected_earn_redeems,
        )
        if any(
            value < 0 or value > limit
            for value, limit in zip(expected_callback_successes, callback_limits)
        ):
            parser.error(
                "expected callback successes must be between zero and the "
                "corresponding expected withdrawal count"
            )
    if args.max_block_range <= 0:
        parser.error("max block range must be positive")
    if args.rpc_timeout <= 0:
        parser.error("RPC timeout must be positive")
    return args


def main() -> int:
    args = parse_args()
    try:
        report = collect(args)
        write_atomic(args.output, json.dumps(report, indent=2, sort_keys=True) + "\n")
        if args.markdown_output is not None:
            write_atomic(args.markdown_output, render_markdown(report))
    except (RpcError, OSError) as error:
        print(f"error: could not collect L1 withdrawal blocks: {error}", file=sys.stderr)
        return 1
    totals = report["totals"]
    print(
        "L1 withdrawal capacity collected: "
        f"{totals['withdrawal_count']} withdrawals in "
        f"{totals['process_tx_count']} process transactions across "
        f"{totals['blocks_with_withdrawals']} blocks"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
