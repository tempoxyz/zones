#!/usr/bin/env python3

from __future__ import annotations

import json
import subprocess
import sys
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[3]
COLLECTOR = ROOT / "contrib/bench/collect-withdrawal-blocks.py"
PORTAL = "0x" + "11" * 20
TOPIC = "0x65042ea6dad60c26f055e80ec401b3437c854ed586a0704d305bb4e9ea4518cf"
SENDER_TOPIC = "0x" + "22" * 32
EARN_ROUTER = "0x" + "33" * 20
BRIDGE = "0x" + "44" * 20
OTHER_DESTINATION = "0x" + "45" * 20
STABLE_TOKEN = "0x" + "55" * 20
EARN_SHARE_TOKEN = "0x" + "66" * 20
BLOCK_100_HASH = "0x" + "a1" * 32
BLOCK_101_HASH = "0x" + "a2" * 32
TX_1 = "0x" + "b1" * 32
TX_2 = "0x" + "b2" * 32
OTHER_TX = "0x" + "b3" * 32


def address_topic(address: str) -> str:
    return "0x" + "00" * 12 + address[2:]


def event_data(token: str, amount: int = 1, callback_success: bool = True) -> str:
    return (
        "0x"
        + "00" * 12
        + token[2:]
        + amount.to_bytes(32, "big").hex()
        + int(callback_success).to_bytes(32, "big").hex()
    )


def event(
    tx_hash: str,
    block: int,
    block_hash: str,
    log_index: int,
    recipient: str,
    token: str,
    callback_success: bool = True,
) -> dict[str, Any]:
    return {
        "address": PORTAL,
        "topics": [TOPIC, address_topic(recipient), SENDER_TOPIC],
        "data": event_data(token, callback_success=callback_success),
        "blockNumber": hex(block),
        "blockHash": block_hash,
        "transactionHash": tx_hash,
        "transactionIndex": "0x0",
        "logIndex": hex(log_index),
        "removed": False,
    }


EVENTS = [
    event(TX_1, 100, BLOCK_100_HASH, 0, EARN_ROUTER, STABLE_TOKEN),
    event(TX_1, 100, BLOCK_100_HASH, 1, EARN_ROUTER, EARN_SHARE_TOKEN),
    event(
        TX_2,
        101,
        BLOCK_101_HASH,
        0,
        BRIDGE,
        STABLE_TOKEN,
        callback_success=False,
    ),
]


class FixtureHandler(BaseHTTPRequestHandler):
    events = EVENTS

    def do_POST(self) -> None:
        length = int(self.headers["Content-Length"])
        request = json.loads(self.rfile.read(length))
        method = request["method"]
        params = request["params"]
        if method == "eth_getLogs":
            result = self.events
        elif method == "eth_getTransactionReceipt":
            tx_hash = params[0]
            if tx_hash == TX_1:
                result = {
                    "transactionHash": TX_1,
                    "blockNumber": "0x64",
                    "blockHash": BLOCK_100_HASH,
                    "status": "0x1",
                    "gasUsed": "0x64",
                    "logs": self.events[:2],
                }
            elif tx_hash == TX_2:
                result = {
                    "transactionHash": TX_2,
                    "blockNumber": "0x65",
                    "blockHash": BLOCK_101_HASH,
                    "status": "0x1",
                    "gasUsed": "0xc8",
                    "logs": self.events[2:],
                }
            else:
                result = None
        elif method == "eth_getTransactionByHash":
            result = {
                "hash": params[0],
                "to": PORTAL,
                "gas": "0x2dc6c0",
                "input": "0x91aa3f04" + "00" * 64,
            }
        elif method == "eth_getBlockByNumber":
            if params[0] == "0x64":
                result = {
                    "number": "0x64",
                    "hash": BLOCK_100_HASH,
                    "gasUsed": "0x3e8",
                    "gasLimit": "0x7530",
                    "transactions": [TX_1, OTHER_TX],
                }
            elif params[0] == "0x65":
                result = {
                    "number": "0x65",
                    "hash": BLOCK_101_HASH,
                    "gasUsed": "0x7d0",
                    "gasLimit": "0x7530",
                    "transactions": [TX_2],
                }
            else:
                result = None
        else:
            self.send_error(500, f"unexpected RPC method {method}")
            return
        body = json.dumps(
            {"jsonrpc": "2.0", "id": request["id"], "result": result}
        ).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, _format: str, *_args: object) -> None:
        pass


class WithdrawalBlocksTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.server = ThreadingHTTPServer(("127.0.0.1", 0), FixtureHandler)
        cls.thread = threading.Thread(target=cls.server.serve_forever, daemon=True)
        cls.thread.start()
        cls.rpc_url = f"http://127.0.0.1:{cls.server.server_port}"

    @classmethod
    def tearDownClass(cls) -> None:
        cls.server.shutdown()
        cls.server.server_close()
        cls.thread.join()

    def run_collector(
        self,
        directory: Path,
        expected_withdrawals: int,
        *,
        expected_classes: tuple[int, int, int] | None = None,
        expected_callback_successes: tuple[int, int] | None = None,
        bridge: str = BRIDGE,
    ) -> subprocess.CompletedProcess[str]:
        command = [
            sys.executable,
            str(COLLECTOR),
            "--rpc-url",
            self.rpc_url,
            "--portal",
            PORTAL,
            "--portal-abi",
            str(ROOT / "contrib/bench/neobank/abis/zone-portal.json"),
            "--from-block",
            "100",
            "--to-block",
            "101",
            "--expected-withdrawals",
            str(expected_withdrawals),
            "--output",
            str(directory / "withdrawal-blocks.json"),
            "--markdown-output",
            str(directory / "withdrawal-blocks.md"),
        ]
        if expected_classes is not None:
            earn_deposits, earn_redeems, offramps = expected_classes
            command.extend(
                [
                    "--earn-router",
                    EARN_ROUTER,
                    "--bridge",
                    bridge,
                    "--stable-token",
                    STABLE_TOKEN,
                    "--earn-share-token",
                    EARN_SHARE_TOKEN,
                    "--expected-earn-deposits",
                    str(earn_deposits),
                    "--expected-earn-redeems",
                    str(earn_redeems),
                    "--expected-offramps",
                    str(offramps),
                ]
            )
        if expected_callback_successes is not None:
            earn_deposit_successes, earn_redeem_successes = (
                expected_callback_successes
            )
            command.extend(
                [
                    "--expected-earn-deposit-callback-successes",
                    str(earn_deposit_successes),
                    "--expected-earn-redeem-callback-successes",
                    str(earn_redeem_successes),
                ]
            )
        return subprocess.run(
            command,
            check=False,
            capture_output=True,
            text=True,
        )

    def test_collects_receipt_scoped_events_by_l1_block(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            output_dir = Path(temporary)
            result = self.run_collector(output_dir, 3)
            self.assertEqual(result.returncode, 0, result.stderr)
            report = json.loads(
                (output_dir / "withdrawal-blocks.json").read_text(encoding="utf-8")
            )
            self.assertEqual(
                report["measured_l1_range"],
                {"start_block": 100, "end_block": 101, "block_count": 2},
            )
            self.assertEqual(
                report["withdrawal_event_span"],
                {"start_block": 100, "end_block": 101, "block_count": 2},
            )
            self.assertEqual(
                report["totals"],
                {
                    "withdrawal_count": 3,
                    "process_tx_count": 2,
                    "process_tx_gas_used": 300,
                    "process_tx_gas_limit": 6_000_000,
                    "blocks_with_withdrawals": 2,
                },
            )
            self.assertEqual(
                report["distribution"]["withdrawals_per_active_block"],
                {"max": 2, "p50": 1, "p95": 2},
            )
            self.assertEqual(
                report["distribution"]["process_tx_gas_used"],
                {"max": 200, "p50": 100, "p95": 200},
            )
            self.assertEqual(
                report["rows"][0],
                {
                    "block_number": 100,
                    "block_hash": BLOCK_100_HASH,
                    "withdrawal_count": 2,
                    "process_tx_count": 1,
                    "process_tx_gas_used": 100,
                    "process_tx_gas_limit": 3_000_000,
                    "l1_block_gas_used": 1000,
                    "l1_block_gas_limit": 30000,
                    "l1_block_gas_utilization_bps": 333,
                    "l1_tx_count": 2,
                    "process_tx_hashes": [TX_1],
                },
            )
            markdown = (output_dir / "withdrawal-blocks.md").read_text(
                encoding="utf-8"
            )
            self.assertIn("Withdrawals per active block", markdown)
            self.assertIn(
                "| 100 | 2 | 1 | 100 | 3000000 | 1000 | 30000 | 2 |",
                markdown,
            )

    def test_classifies_expected_withdrawal_routes(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            output_dir = Path(temporary)
            result = self.run_collector(
                output_dir,
                3,
                expected_classes=(1, 1, 1),
                expected_callback_successes=(1, 1),
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            report = json.loads(
                (output_dir / "withdrawal-blocks.json").read_text(encoding="utf-8")
            )
            classes = report["withdrawal_classes"]["classes"]
            self.assertEqual(
                {
                    name: details["withdrawal_count"]
                    for name, details in classes.items()
                },
                {"earn_deposit": 1, "earn_redeem": 1, "offramp": 1},
            )
            self.assertEqual(
                classes["earn_deposit"]["distribution"][
                    "withdrawals_per_measured_block"
                ],
                {"max": 1, "p50": 0, "p95": 1},
            )
            self.assertEqual(
                classes["offramp"]["busiest_blocks"],
                [
                    {
                        "block_number": 101,
                        "withdrawal_count": 1,
                        "process_tx_count": 1,
                        "process_tx_gas_used": 200,
                        "l1_block_gas_used": 2000,
                        "l1_block_gas_limit": 30000,
                    }
                ],
            )
            self.assertEqual(
                classes["earn_deposit"]["callback_outcomes"],
                {
                    "applicable": True,
                    "success_count": 1,
                    "failure_count": 0,
                    "expected_success_count": 1,
                    "expected_failure_count": 0,
                },
            )
            self.assertEqual(
                classes["offramp"]["callback_outcomes"],
                {"applicable": False},
            )
            self.assertEqual(
                report["rows"][0]["class_counts"],
                {"earn_deposit": 1, "earn_redeem": 1, "offramp": 0},
            )
            markdown = (output_dir / "withdrawal-blocks.md").read_text(
                encoding="utf-8"
            )
            self.assertIn("### Withdrawal classes", markdown)
            self.assertIn("| earn deposit | 1 | 1 / 0 | 1 |", markdown)
            self.assertIn("| offramp | 1 | n/a (no callback) | 1 |", markdown)
            self.assertIn("| offramp | 101 | 1 |", markdown)

    def test_accepts_expected_failed_earn_callback(self) -> None:
        failed_deposit_events = [
            event(
                TX_1,
                100,
                BLOCK_100_HASH,
                0,
                EARN_ROUTER,
                STABLE_TOKEN,
                callback_success=False,
            ),
            EVENTS[1],
            EVENTS[2],
        ]
        FixtureHandler.events = failed_deposit_events
        try:
            with tempfile.TemporaryDirectory() as temporary:
                output_dir = Path(temporary)
                result = self.run_collector(
                    output_dir,
                    3,
                    expected_classes=(1, 1, 1),
                    expected_callback_successes=(0, 1),
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                report = json.loads(
                    (output_dir / "withdrawal-blocks.json").read_text(
                        encoding="utf-8"
                    )
                )
                self.assertEqual(
                    report["withdrawal_classes"]["classes"]["earn_deposit"][
                        "callback_outcomes"
                    ],
                    {
                        "applicable": True,
                        "success_count": 0,
                        "failure_count": 1,
                        "expected_success_count": 0,
                        "expected_failure_count": 1,
                    },
                )
        finally:
            FixtureHandler.events = EVENTS

    def test_rejects_unexpected_failed_earn_callback(self) -> None:
        cases = (
            (
                "earn_deposit",
                [
                    event(
                        TX_1,
                        100,
                        BLOCK_100_HASH,
                        0,
                        EARN_ROUTER,
                        STABLE_TOKEN,
                        callback_success=False,
                    ),
                    EVENTS[1],
                    EVENTS[2],
                ],
            ),
            (
                "earn_redeem",
                [
                    EVENTS[0],
                    event(
                        TX_1,
                        100,
                        BLOCK_100_HASH,
                        1,
                        EARN_ROUTER,
                        EARN_SHARE_TOKEN,
                        callback_success=False,
                    ),
                    EVENTS[2],
                ],
            ),
        )
        for withdrawal_class, failed_events in cases:
            with self.subTest(withdrawal_class=withdrawal_class):
                FixtureHandler.events = failed_events
                try:
                    with tempfile.TemporaryDirectory() as temporary:
                        result = self.run_collector(
                            Path(temporary),
                            3,
                            expected_classes=(1, 1, 1),
                            expected_callback_successes=(1, 1),
                        )
                        self.assertNotEqual(result.returncode, 0)
                        self.assertIn(
                            f"{withdrawal_class} callback outcomes success/failure "
                            "0/1 do not equal expected 1/0",
                            result.stderr,
                        )
                finally:
                    FixtureHandler.events = EVENTS

    def test_rejects_unexpected_withdrawal_route(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            result = self.run_collector(
                Path(temporary),
                3,
                expected_classes=(1, 1, 1),
                bridge=OTHER_DESTINATION,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("unexpected WithdrawalProcessed route", result.stderr)

    def test_rejects_incorrect_per_class_count(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            result = self.run_collector(
                Path(temporary), 3, expected_classes=(2, 0, 1)
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn(
                "earn_deposit WithdrawalProcessed count 1 does not equal expected 2",
                result.stderr,
            )

    def test_rejects_an_incomplete_event_set(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            result = self.run_collector(Path(temporary), 4)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn(
                "WithdrawalProcessed count 3 does not equal expected 4", result.stderr
            )


if __name__ == "__main__":
    unittest.main()
