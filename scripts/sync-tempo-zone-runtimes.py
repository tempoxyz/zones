#!/usr/bin/env python3
"""Update Tempo's embedded Zone runtimes when their executable code changes."""

import argparse
import hashlib
import json
from pathlib import Path

CONSTANTS = {
    "ZonePortal": "ZONE_PORTAL_RUNTIME",
    "ZoneMessenger": "ZONE_MESSENGER_RUNTIME",
}


def runtime(body: str) -> bytes:
    encoded = "".join(line.strip().strip('"') for line in body.splitlines())
    return bytes.fromhex(encoded.removeprefix("0x"))


def executable(code: bytes) -> bytes:
    """Remove the CBOR metadata whose length is stored in the final two bytes."""
    return code[: -(int.from_bytes(code[-2:], "big") + 2)]


def locate(source: str, constant: str) -> tuple[int, int, int]:
    marker = f"pub const {constant}: Bytes = bytes!(\n"
    start = source.find(marker)
    if start == -1:
        raise SystemExit(f"{constant} not found in Tempo")
    body_start = start + len(marker)
    end = source.find(");", body_start)
    if end == -1:
        raise SystemExit(f"{constant} is missing its closing delimiter")
    return start, body_start, end + 2


def declaration(constant: str, compiled: bytes) -> str:
    encoded = compiled.hex()
    chunks = [encoded[i : i + 160] for i in range(0, len(encoded), 160)]
    body = "".join(f'    "{"0x" if i == 0 else ""}{chunk}"\n' for i, chunk in enumerate(chunks))
    return f"pub const {constant}: Bytes = bytes!(\n{body});"


def replace(
    source: str, contract: str, constant: str, compiled: bytes, hardfork: bool, zones_sha: str
) -> tuple[str, bool]:
    candidate = f"{constant}_TN"
    target = candidate if hardfork and f"pub const {candidate}" in source else constant
    start, body_start, end = locate(source, target)
    expected = executable(compiled)
    actual = executable(runtime(source[body_start : end - 2]))
    print(f"{target}: {hashlib.sha256(actual).hexdigest()} → {hashlib.sha256(expected).hexdigest()}")
    if actual == expected:
        return source, False

    updated = declaration(candidate if hardfork else constant, compiled)
    if not hardfork:
        return source[:start] + updated + source[end:], True

    todo = (
        f"// TODO(hardfork): Gate this {contract} runtime update behind the next Tempo "
        "hardfork (TN) before merging.\n//\n"
        f"// Source: tempoxyz/zones@{zones_sha}\n"
    )
    if target == candidate:
        todo_start = source.rfind("// TODO(hardfork):", 0, start)
        start = todo_start if todo_start != -1 else start
        return source[:start] + todo + updated + source[end:], True

    return source[:end] + "\n\n" + todo + updated + source[end:], True


parser = argparse.ArgumentParser()
parser.add_argument("--runtime", action="append", required=True, metavar="CONTRACT=FILE")
parser.add_argument("--tempo-file", type=Path, required=True)
parser.add_argument("--presto", type=Path, required=True)
parser.add_argument("--zones-sha", required=True)
parser.add_argument("--github-output", type=Path, required=True)
args = parser.parse_args()

hardfork = "t9Time" in json.loads(args.presto.read_text())["config"]
source = args.tempo_file.read_text()
changed = False
for value in args.runtime:
    contract, path = value.split("=", 1)
    source, runtime_changed = replace(
        source,
        contract,
        CONSTANTS[contract],
        bytes.fromhex(Path(path).read_text().strip().removeprefix("0x")),
        hardfork,
        args.zones_sha,
    )
    changed |= runtime_changed

if changed:
    args.tempo_file.write_text(source)
with args.github_output.open("a") as output:
    output.write(f"changed={str(changed).lower()}\n")
    output.write(f"hardfork={str(hardfork).lower()}\n")
