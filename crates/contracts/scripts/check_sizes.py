#!/usr/bin/env python3
"""Report production bytecode sizes; enforce creation limits only for CREATE contracts."""
import argparse
import hashlib
import json
from pathlib import Path

# These exact artifacts are installed into account state by Tempo's hardfork hooks.
PROTOCOL_RUNTIMES = {
    ("src/runtime/tempo/ZonePortal.sol", "ZonePortal"): "0x5ad1000000000000000000000000000000000000",
    ("src/runtime/tempo/ZoneMessenger.sol", "ZoneMessenger"): "0x5a4d000000000000000000000000000000000000",
    ("src/runtime/tempo/Verifier.sol", "Verifier"): "0x5a56000000000000000000000000000000000000",
}
REQUIRED = set(PROTOCOL_RUNTIMES) | {("src/runtime/tempo/SwapAndDepositRouter.sol", "SwapAndDepositRouter")}


def bytecode(artifact, field, protocol=False):
    code = artifact[field]
    if code.get("linkReferences") or (protocol and code.get("immutableReferences")):
        raise ValueError(f"unresolved links/immutables in {field}")
    return bytes.fromhex(code["object"].removeprefix("0x"))


def check_artifact(source, name, artifact):
    runtime = bytecode(artifact, "deployedBytecode", (source, name) in PROTOCOL_RUNTIMES)
    # Constructor-patched immutables are valid for normally deployed contracts.
    initcode = bytecode(artifact, "bytecode")
    if not runtime:
        if (source, name) in REQUIRED:
            raise ValueError("required runtime is empty")
        return None
    if (source, name) in PROTOCOL_RUNTIMES:
        mode = f"protocol-installed at {PROTOCOL_RUNTIMES[source, name]} (size informational)"
    else:
        mode = "CREATE"
        if len(runtime) > 24576 or len(initcode) > 49152:
            raise ValueError(f"creation size limit exceeded: runtime={len(runtime)}, initcode={len(initcode)}")
    return (f"{source}:{name}: runtime={len(runtime)}, initcode={len(initcode)}; {mode}; "
            f"runtime-sha256={hashlib.sha256(runtime).hexdigest()}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", type=Path, default=Path("out"))
    args = parser.parse_args()
    seen = set()
    errors = []
    for path in sorted(args.out.glob("*/*.json")):
        if path.parent.name == "build-info":
            continue
        try:
            artifact = json.loads(path.read_text())
            targets = artifact["metadata"]["settings"]["compilationTarget"]
            for source, name in targets.items():
                if not source.startswith("src/runtime/"):
                    continue
                seen.add((source, name))
                report = check_artifact(source, name, artifact)
                if report:
                    print(report)
        except (ValueError, KeyError, TypeError) as error:
            errors.append(f"{path}: {error}")
    errors.extend(f"missing production artifact: {source}:{name}" for source, name in sorted(REQUIRED - seen))
    if errors:
        parser.exit(1, "\n".join(errors) + "\n")


if __name__ == "__main__":
    main()
