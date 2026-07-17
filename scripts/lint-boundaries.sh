#!/usr/bin/env bash
# Structural lints: the machine-checked boundaries from docs/standards.md.
set -euo pipefail
cd "$(dirname "$0")/.."

# 1. crown-reduce has zero dependencies: its tree is exactly one line — itself.
tree="$(cargo tree -p crown-reduce --edges normal --prefix none)"
if [ "$(printf '%s\n' "$tree" | wc -l)" -ne 1 ]; then
    echo "FAIL: crown-reduce must have zero dependencies, got:" >&2
    printf '%s\n' "$tree" >&2
    exit 1
fi

# 2. reduce sources never touch I/O or chain SDKs.
if grep -rEn 'ic_cdk|std::(fs|net|time)|reqwest' reduce/src/; then
    echo "FAIL: forbidden reference in reduce/src" >&2
    exit 1
fi

# 3. mainnet profile carries no Custom RPC sources (comments don't count).
if grep -v '^[[:space:]]*#' config/mainnet.toml | grep -n 'Custom'; then
    echo "FAIL: Custom RPC source in mainnet profile" >&2
    exit 1
fi

# 4. nobody can write to the canister: the only non-query .did method is the
#    empty alarm clock ingest_hint (docs/core-spec.md §5).
if grep '\->' index/crown-index.did | grep -v 'service :' | grep -v 'query' \
    | grep -vnE '^[[:space:]]*ingest_hint[[:space:]]*:[[:space:]]*\(\)'; then
    echo "FAIL: unexpected non-query method in crown-index.did" >&2
    exit 1
fi

echo "boundaries OK"
