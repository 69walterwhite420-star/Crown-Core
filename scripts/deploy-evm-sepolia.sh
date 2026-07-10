#!/usr/bin/env bash
# Deploys the immutable splitter to Ethereum Sepolia with constructor args
# from contracts/evm/deploy.toml and (optionally) verifies it on Etherscan.
#
# Usage:
#   PRIVATE_KEY=0x... [ETHERSCAN_API_KEY=...] scripts/deploy-evm-sepolia.sh [rpc-url]
#
# After deploying: put the printed address into config/testnet.toml as the
# eth-sepolia splitter, run scripts/lint-evm-opcodes.sh, commit both together.
set -euo pipefail
cd "$(dirname "$0")/../contracts/evm"

RPC=${1:-https://ethereum-sepolia-rpc.publicnode.com}
: "${PRIVATE_KEY:?set PRIVATE_KEY (see ~/.cache/crown-e2e/evm-deployer.key)}"

value_of() { grep "^$1" deploy.toml | cut -d'=' -f2 | tr -d ' "'; }
FEE_BPS=$(value_of fee_bps)
TREASURY=$(value_of treasury)
USDC=$(value_of usdc)
echo "fee_bps=$FEE_BPS treasury=$TREASURY usdc=$USDC"

VERIFY=()
if [ -n "${ETHERSCAN_API_KEY:-}" ]; then
    VERIFY=(--verify --etherscan-api-key "$ETHERSCAN_API_KEY")
fi

forge create src/Splitter.sol:Splitter \
    --rpc-url "$RPC" \
    --private-key "$PRIVATE_KEY" \
    --broadcast \
    --constructor-args "$FEE_BPS" "$TREASURY" "$USDC" \
    "${VERIFY[@]}"
