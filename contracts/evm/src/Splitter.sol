// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {IERC20} from "openzeppelin-contracts/contracts/token/ERC20/IERC20.sol";
import {IERC20Permit} from "openzeppelin-contracts/contracts/token/ERC20/extensions/IERC20Permit.sol";
import {SafeERC20} from "openzeppelin-contracts/contracts/token/ERC20/utils/SafeERC20.sol";

/// @notice Crown splitter: immutable 97/3 split executed inside the donor's
/// transaction (docs/core-spec.md §3). The contract never owns tokens: two
/// direct payer -> recipient transfers. No owner, no admin, no proxy, no
/// delegatecall, no selfdestruct — nothing to upgrade and nothing to steal.
contract Splitter {
    using SafeERC20 for IERC20;

    uint256 public constant BPS_DENOMINATOR = 10_000;

    uint256 public immutable FEE_BPS;
    address public immutable TREASURY;
    IERC20 public immutable USDC;

    /// @notice The settlement, as the indexer reads it back from the chain.
    event Settled(address indexed payer, address indexed streamer, uint256 gross, uint256 fee);

    /// @notice gross so small the fee rounds to zero: reputation would be free.
    error BelowFeeFloor();

    constructor(uint256 feeBps, address treasury, IERC20 usdc) {
        require(feeBps > 0 && feeBps < BPS_DENOMINATOR, "fee out of range");
        require(treasury != address(0), "zero treasury");
        require(address(usdc) != address(0), "zero usdc");
        FEE_BPS = feeBps;
        TREASURY = treasury;
        USDC = usdc;
    }

    /// @notice Splits `gross` USDC minor units from the caller: payout straight
    /// to the streamer, fee straight to the treasury, then one Settled event.
    /// The payer is structurally msg.sender — tokens move only from the caller,
    /// so reputation cannot be gifted to a wallet that did not pay.
    function donate(address streamer, uint256 gross) external {
        _donate(streamer, gross);
    }

    /// @notice Same, with an EIP-2612 permit so no separate approve is needed.
    /// The permit call may fail (e.g. it was front-run and the nonce is spent);
    /// the donate still succeeds as long as the allowance is in place.
    function donateWithPermit(address streamer, uint256 gross, uint256 deadline, uint8 v, bytes32 r, bytes32 s)
        external
    {
        try IERC20Permit(address(USDC)).permit(msg.sender, address(this), gross, deadline, v, r, s) {} catch {}
        _donate(streamer, gross);
    }

    function _donate(address streamer, uint256 gross) internal {
        uint256 fee = (gross * FEE_BPS) / BPS_DENOMINATOR;
        if (fee == 0) revert BelowFeeFloor();
        USDC.safeTransferFrom(msg.sender, streamer, gross - fee);
        USDC.safeTransferFrom(msg.sender, TREASURY, fee);
        emit Settled(msg.sender, streamer, gross, fee);
    }
}
