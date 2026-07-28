#!/usr/bin/env python3
"""Compare a compiled ZonePortal runtime with Tempo and update Tempo on drift."""

from __future__ import annotations

import argparse
import hashlib
import re
import sys
from pathlib import Path

CONST_RE = re.compile(
    r"(?P<prefix>pub const ZONE_PORTAL_RUNTIME: Bytes = bytes!\(\n)"
    r"(?P<body>(?:\s*\"[0-9a-fA-Fx]+\"\n)+)"
    r"(?P<suffix>\);)"
)
HEX_RE = re.compile(r'"([0-9a-fA-Fx]+)"')


def decode_runtime(value: str, source: str) -> bytes:
    value = "".join(value.split()).removeprefix("0x")
    if not value or len(value) % 2 or not re.fullmatch(r"[0-9a-fA-F]+", value):
        raise SystemExit(f"{source} is not non-empty hex bytecode")
    return bytes.fromhex(value)


def executable(runtime: bytes, source: str) -> bytes:
    if len(runtime) < 2:
        raise SystemExit(f"{source} is too short to contain Solidity metadata")
    metadata_length = int.from_bytes(runtime[-2:], "big")
    executable_length = len(runtime) - metadata_length - 2
    if metadata_length == 0 or executable_length <= 0:
        raise SystemExit(f"{source} has an invalid Solidity metadata trailer")
    return runtime[:executable_length]


def digest(value: bytes) -> str:
    # SHA-256 is only used for stable CI diagnostics; equality compares the bytes.
    return hashlib.sha256(value).hexdigest()


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--runtime-file", type=Path, required=True)
    parser.add_argument("--tempo-file", type=Path, required=True)
    parser.add_argument("--github-output", type=Path)
    args = parser.parse_args()

    compiled = decode_runtime(args.runtime_file.read_text(), "compiled ZonePortal runtime")
    source = args.tempo_file.read_text()
    match = CONST_RE.search(source)
    if not match:
        raise SystemExit(f"could not find ZONE_PORTAL_RUNTIME in {args.tempo_file}")

    embedded_hex = "".join(HEX_RE.findall(match.group("body")))
    embedded = decode_runtime(embedded_hex, "Tempo ZONE_PORTAL_RUNTIME")
    compiled_executable = executable(compiled, "compiled ZonePortal runtime")
    embedded_executable = executable(embedded, "Tempo ZONE_PORTAL_RUNTIME")
    changed = compiled_executable != embedded_executable

    print(f"Zones executable sha256: {digest(compiled_executable)}")
    print(f"Tempo executable sha256: {digest(embedded_executable)}")

    if changed:
        encoded = compiled.hex()
        chunks = [encoded[index : index + 160] for index in range(0, len(encoded), 160)]
        body = "".join(
            f'    "{("0x" if index == 0 else "")}{chunk}"\n'
            for index, chunk in enumerate(chunks)
        )
        replacement = match.group("prefix") + body + match.group("suffix")
        args.tempo_file.write_text(source[: match.start()] + replacement + source[match.end() :])
        print(f"Updated {args.tempo_file}")
    else:
        print("Tempo already contains the current ZonePortal executable")

    if args.github_output:
        with args.github_output.open("a") as output:
            output.write(f"changed={'true' if changed else 'false'}\n")
            output.write(f"zones_hash={digest(compiled_executable)}\n")
            output.write(f"tempo_hash={digest(embedded_executable)}\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
