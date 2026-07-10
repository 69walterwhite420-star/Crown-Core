// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {Test} from "forge-std/Test.sol";
import {IERC20} from "openzeppelin-contracts/contracts/token/ERC20/IERC20.sol";
import {ERC20} from "openzeppelin-contracts/contracts/token/ERC20/ERC20.sol";
import {ERC20Permit} from "openzeppelin-contracts/contracts/token/ERC20/extensions/ERC20Permit.sol";

import {Splitter} from "../src/Splitter.sol";

contract MockUSDC is ERC20Permit {
    constructor() ERC20("USD Coin", "USDC") ERC20Permit("USD Coin") {}

    function decimals() public pure override returns (uint8) {
        return 6;
    }

    function mint(address to, uint256 amount) external {
        _mint(to, amount);
    }
}

contract SplitterTest is Test {
    uint256 constant FEE_BPS = 300;
    uint256 constant DONOR_KEY = 0xD0102;

    MockUSDC usdc;
    Splitter splitter;
    address donor;
    address streamer = makeAddr("streamer");
    address treasury = makeAddr("treasury");

    function setUp() public {
        donor = vm.addr(DONOR_KEY);
        usdc = new MockUSDC();
        splitter = new Splitter(FEE_BPS, treasury, IERC20(address(usdc)));
        usdc.mint(donor, type(uint128).max);
        vm.prank(donor);
        usdc.approve(address(splitter), type(uint256).max);
    }

    // out == in: payout + fee == gross exactly, over the whole sane range.
    function testFuzz_donateSplitsGrossExactly(uint128 gross) public {
        gross = uint128(bound(gross, 34, type(uint128).max));
        uint256 donorBefore = usdc.balanceOf(donor);

        vm.prank(donor);
        splitter.donate(streamer, gross);

        uint256 fee = (uint256(gross) * FEE_BPS) / splitter.BPS_DENOMINATOR();
        assertEq(usdc.balanceOf(streamer), gross - fee, "payout");
        assertEq(usdc.balanceOf(treasury), fee, "fee");
        assertEq(usdc.balanceOf(donor), donorBefore - gross, "payer debit");
        assertEq(usdc.balanceOf(streamer) + usdc.balanceOf(treasury), gross, "out == in");
    }

    // Fee floor: everything below the smallest gross with fee > 0 reverts.
    function testFuzz_belowFeeFloorReverts(uint256 gross) public {
        gross = bound(gross, 0, 33); // fee = gross * 300 / 10000 == 0
        vm.prank(donor);
        vm.expectRevert(Splitter.BelowFeeFloor.selector);
        splitter.donate(streamer, gross);
    }

    function test_feeFloorBoundary() public {
        vm.prank(donor);
        splitter.donate(streamer, 34);
        assertEq(usdc.balanceOf(treasury), 1);
        assertEq(usdc.balanceOf(streamer), 33);
    }

    // Zero balance invariant: the contract owns nothing after any donate flow.
    function testFuzz_splitterBalanceAlwaysZero(uint64[8] calldata grosses) public {
        for (uint256 i = 0; i < grosses.length; i++) {
            uint256 gross = bound(grosses[i], 34, type(uint64).max);
            vm.prank(donor);
            splitter.donate(streamer, gross);
            assertEq(usdc.balanceOf(address(splitter)), 0, "splitter must never hold funds");
        }
    }

    // Structural payer: an approval to the splitter cannot be spent by anyone
    // else — the contract always pulls from msg.sender.
    function test_cannotSpendSomeoneElsesAllowance() public {
        address attacker = makeAddr("attacker");
        vm.prank(attacker);
        vm.expectRevert(); // attacker has no funds and no allowance
        splitter.donate(attacker, 1_000_000);
        assertEq(usdc.balanceOf(attacker), 0);
    }

    function test_settledEventCarriesExactAmounts() public {
        vm.expectEmit(true, true, false, true, address(splitter));
        emit Splitter.Settled(donor, streamer, 1_000_000, 30_000);
        vm.prank(donor);
        splitter.donate(streamer, 1_000_000);
    }

    function _signPermit(uint256 gross, uint256 deadline) internal view returns (uint8 v, bytes32 r, bytes32 s) {
        bytes32 structHash = keccak256(
            abi.encode(
                keccak256("Permit(address owner,address spender,uint256 value,uint256 nonce,uint256 deadline)"),
                donor,
                address(splitter),
                gross,
                usdc.nonces(donor),
                deadline
            )
        );
        bytes32 digest = keccak256(abi.encodePacked("\x19\x01", usdc.DOMAIN_SEPARATOR(), structHash));
        return vm.sign(DONOR_KEY, digest);
    }

    // Permit path: donate with no prior approve.
    function test_donateWithPermit() public {
        vm.prank(donor);
        usdc.approve(address(splitter), 0);
        (uint8 v, bytes32 r, bytes32 s) = _signPermit(1_000_000, block.timestamp + 1);

        vm.prank(donor);
        splitter.donateWithPermit(streamer, 1_000_000, block.timestamp + 1, v, r, s);
        assertEq(usdc.balanceOf(streamer), 970_000);
        assertEq(usdc.balanceOf(treasury), 30_000);
    }

    // A front-run permit must not brick the donate: the allowance is already
    // in place, the failing permit call is swallowed.
    function test_donateWithFrontRunPermit() public {
        vm.prank(donor);
        usdc.approve(address(splitter), 0);
        (uint8 v, bytes32 r, bytes32 s) = _signPermit(1_000_000, block.timestamp + 1);
        // Front-runner spends the permit before the donor's transaction.
        usdc.permit(donor, address(splitter), 1_000_000, block.timestamp + 1, v, r, s);

        vm.prank(donor);
        splitter.donateWithPermit(streamer, 1_000_000, block.timestamp + 1, v, r, s);
        assertEq(usdc.balanceOf(streamer), 970_000);
    }

    function test_constructorRejectsBadParams() public {
        vm.expectRevert("fee out of range");
        new Splitter(0, treasury, IERC20(address(usdc)));
        vm.expectRevert("fee out of range");
        new Splitter(10_000, treasury, IERC20(address(usdc)));
        vm.expectRevert("zero treasury");
        new Splitter(300, address(0), IERC20(address(usdc)));
        vm.expectRevert("zero usdc");
        new Splitter(300, treasury, IERC20(address(0)));
    }
}
