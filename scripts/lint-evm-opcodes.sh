#!/usr/bin/env bash
# Immutability lint for the EVM splitter (docs/build-plan.md S4): the runtime
# bytecode must contain no SELFDESTRUCT, DELEGATECALL or CALLCODE opcodes.
# Requires foundry. Walks real opcodes, skipping PUSH data, on metadata-free
# bytecode (cbor_metadata = false in foundry.toml).
set -euo pipefail
cd "$(dirname "$0")/../contracts/evm"

forge build --quiet
forge inspect src/Splitter.sol:Splitter deployedBytecode | python3 -c '
import sys

code = bytes.fromhex(sys.stdin.read().strip().removeprefix("0x"))
FORBIDDEN = {0xFF: "SELFDESTRUCT", 0xF4: "DELEGATECALL", 0xF2: "CALLCODE"}
i, bad = 0, []
while i < len(code):
    op = code[i]
    if op in FORBIDDEN:
        bad.append(f"offset {i}: {FORBIDDEN[op]}")
    # PUSH1..PUSH32 carry 1..32 bytes of immediate data
    i += 1 + (op - 0x5F if 0x60 <= op <= 0x7F else 0)
if bad:
    print("FAIL: forbidden opcodes in Splitter runtime bytecode:", file=sys.stderr)
    print("\n".join(bad), file=sys.stderr)
    sys.exit(1)
print("opcodes OK")
'
