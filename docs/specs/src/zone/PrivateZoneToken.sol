// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { IZoneConfig, ZONE_INBOX, ZONE_OUTBOX } from "./IZone.sol";

/**
 * @title PrivateZoneToken
 * @notice Reference specification for the TIP-20 precompile modifications on a privacy zone.
 *
 * @dev This is NOT an actual implementation — the zone token is a precompile. This contract
 * exists to clearly document the behavioral differences from the standard TIP-20 spec.
 *
 * A privacy zone modifies the zone token in four areas:
 *
 *   1. **Balance privacy**: balanceOf() is restricted to the account owner and sequencer.
 *   2. **Allowance privacy**: allowance() is restricted to the owner, spender, and sequencer.
 *   3. **Fixed gas**: All transfer-family calls and approve() cost exactly FIXED_TRANSFER_GAS.
 *   4. **System mint/burn**: ZoneInbox can mint (deposits), ZoneOutbox can burn (withdrawals),
 *      without requiring the standard ISSUER_ROLE.
 *
 * All other TIP-20 behavior (transfer logic, approval logic, events, roles, TIP-403 enforcement,
 * pause controls, rewards) is identical to the standard TIP-20 spec.
 */
abstract contract PrivateZoneToken {

    /*//////////////////////////////////////////////////////////////
                               CONSTANTS
    //////////////////////////////////////////////////////////////*/

    /// @notice Fixed gas cost for all transfer-family operations.
    /// @dev Prevents account-existence side channels via gas consumption differences.
    ///      Matches the zone's FIXED_DEPOSIT_GAS for consistency.
    uint256 public constant FIXED_TRANSFER_GAS = 100_000;

    /// @notice Zone configuration (provides sequencer address from L1).
    IZoneConfig public immutable config;

    /*//////////////////////////////////////////////////////////////
                                ERRORS
    //////////////////////////////////////////////////////////////*/

    /// @notice Caller is not authorized to read this account's data.
    error Unauthorized();

    /*//////////////////////////////////////////////////////////////
                          1. BALANCE PRIVACY
    //////////////////////////////////////////////////////////////*/

    /**
     * @notice Returns the balance of `account`.
     * @dev On a standard zone (and on Tempo), this is public. On a privacy zone:
     *      - msg.sender == account: allowed (self-query)
     *      - msg.sender == sequencer: allowed (block production, fee accounting)
     *      - otherwise: reverts with Unauthorized()
     *
     *      This is enforced at the precompile level so that even on-chain contracts
     *      cannot read arbitrary balances.
     */
    function balanceOf(address account) external view returns (uint256) {
        if (msg.sender != account && msg.sender != config.sequencer()) {
            revert Unauthorized();
        }

        revert(); // precompile stub — actual logic is in the precompile
    }

    /*//////////////////////////////////////////////////////////////
                         2. ALLOWANCE PRIVACY
    //////////////////////////////////////////////////////////////*/

    /**
     * @notice Returns the allowance that `owner` has granted to `spender`.
     * @dev On a standard zone, this is public. On a privacy zone:
     *      - msg.sender == owner: allowed
     *      - msg.sender == spender: allowed
     *      - msg.sender == sequencer: allowed
     *      - otherwise: reverts with Unauthorized()
     *
     *      Rationale: A non-zero allowance reveals that owner has interacted with spender.
     *      Both parties can still check the allowance for standard ERC-20 approval flows.
     */
    function allowance(address owner, address spender) external view returns (uint256) {
        if (msg.sender != owner && msg.sender != spender && msg.sender != config.sequencer()) {
            revert Unauthorized();
        }

        revert(); // precompile stub — actual logic is in the precompile
    }

    /*//////////////////////////////////////////////////////////////
                        3. FIXED TRANSFER GAS
    //////////////////////////////////////////////////////////////*/

    /**
     * @notice Transfer tokens to `to`.
     * @dev Charges exactly FIXED_TRANSFER_GAS regardless of storage operations.
     *
     *      On a standard EVM, transferring to a new account (zero → non-zero storage)
     *      costs ~20k more gas than transferring to an existing account. This difference
     *      reveals whether the recipient has previously received tokens.
     *
     *      The precompile always charges FIXED_TRANSFER_GAS. If the caller provides less
     *      gas, the call reverts. Excess gas is returned.
     *
     *      All other transfer logic (balance checks, TIP-403 policy, pause, events)
     *      is identical to the standard TIP-20.
     */
    function transfer(address to, uint256 amount) external returns (bool) {
        // Precompile charges exactly FIXED_TRANSFER_GAS
        revert(); // precompile stub
    }

    /// @dev Same fixed gas cost as transfer().
    function transferFrom(address from, address to, uint256 amount) external returns (bool) {
        // Precompile charges exactly FIXED_TRANSFER_GAS
        revert(); // precompile stub
    }

    /// @dev Same fixed gas cost as transfer().
    function transferWithMemo(address to, uint256 amount, bytes32 memo) external {
        // Precompile charges exactly FIXED_TRANSFER_GAS
        revert(); // precompile stub
    }

    /// @dev Same fixed gas cost as transfer().
    function transferFromWithMemo(
        address from,
        address to,
        uint256 amount,
        bytes32 memo
    )
        external
        returns (bool)
    {
        // Precompile charges exactly FIXED_TRANSFER_GAS
        revert(); // precompile stub
    }

    /**
     * @notice Approve `spender` to spend `amount` on behalf of the caller.
     * @dev Charges exactly FIXED_TRANSFER_GAS regardless of whether the previous
     *      allowance was zero or non-zero. Without fixed gas, the ~15k gas difference
     *      between a new approval (zero → non-zero storage) and an update (non-zero →
     *      non-zero) would be visible in the sender's receipt, leaking whether a prior
     *      approval existed between the two parties.
     */
    function approve(address spender, uint256 amount) external returns (bool) {
        // Precompile charges exactly FIXED_TRANSFER_GAS
        revert(); // precompile stub
    }

    /*//////////////////////////////////////////////////////////////
                        4. SYSTEM MINT / BURN
    //////////////////////////////////////////////////////////////*/

    /**
     * @notice Mint tokens to `to`.
     * @dev On Tempo, mint() requires the caller to have ISSUER_ROLE. On a privacy zone,
     *      mint is additionally callable by the ZoneInbox system contract (for deposit
     *      processing). The ZoneInbox needs to mint when:
     *        - A regular deposit is processed (mint to recipient)
     *        - An encrypted deposit is successfully decrypted (mint to decrypted recipient)
     *        - An encrypted deposit fails decryption (refund mint to sender)
     *
     *      Access: ISSUER_ROLE holders OR ZoneInbox (0x1c...0001).
     *      Gas: standard (not fixed) — only the sequencer calls this.
     */
    function mint(address to, uint256 amount) external {
        _requireMintAuth();
        revert(); // precompile stub
    }

    /**
     * @notice Burn tokens from `from`.
     * @dev On Tempo, burn() requires the caller to have ISSUER_ROLE. On a privacy zone,
     *      burn is additionally callable by the ZoneOutbox system contract (for withdrawal
     *      processing). The ZoneOutbox flow is:
     *        1. User approves ZoneOutbox to spend (amount + fee)
     *        2. ZoneOutbox calls transferFrom(user, self, amount + fee)
     *        3. ZoneOutbox calls burn(self, amount + fee)
     *      The burned tokens are released on Tempo when the withdrawal is processed.
     *
     *      Access: ISSUER_ROLE holders OR ZoneOutbox (0x1c...0002).
     *      Gas: standard (not fixed) — only the sequencer/outbox calls this.
     */
    function burn(address from, uint256 amount) external {
        _requireBurnAuth();
        revert(); // precompile stub
    }

    /**
     * @dev Check that the caller is authorized to mint.
     *      On a privacy zone, this is: ISSUER_ROLE OR ZoneInbox.
     *
     *      Note: On a zone, no external party typically holds ISSUER_ROLE (the zone token
     *      is a bridged representation). The system contracts are the primary minters/burners.
     *      ISSUER_ROLE is preserved for forward compatibility.
     */
    function _requireMintAuth() internal view {
        if (
            msg.sender != ZONE_INBOX
            // && !hasRole(ISSUER_ROLE, msg.sender)  // standard role check preserved
        ) {
            revert Unauthorized();
        }
    }

    /**
     * @dev Check that the caller is authorized to burn.
     *      On a privacy zone, this is: ISSUER_ROLE OR ZoneOutbox.
     */
    function _requireBurnAuth() internal view {
        if (
            msg.sender != ZONE_OUTBOX
            // && !hasRole(ISSUER_ROLE, msg.sender)  // standard role check preserved
        ) {
            revert Unauthorized();
        }
    }

    /*//////////////////////////////////////////////////////////////
                        UNCHANGED FROM STANDARD TIP-20
    //////////////////////////////////////////////////////////////*/

    // The following functions are UNCHANGED from the standard TIP-20 spec:
    //
    //   View functions (public, no access control):
    //     - name(), symbol(), decimals(), totalSupply()
    //     - paused(), supplyCap(), currency(), quoteToken(), transferPolicyId()
    //
    //   State-changing functions (standard access control):
    //     - burnBlocked(from, amount)            — requires BURN_BLOCKED_ROLE
    //     - pause() / unpause()                  — requires PAUSE_ROLE / UNPAUSE_ROLE
    //     - changeTransferPolicyId(newPolicyId)   — requires DEFAULT_ADMIN_ROLE
    //     - grantRole / revokeRole / renounceRole — standard role management
    //
    //   System functions (precompile-only callers):
    //     - systemTransferFrom(from, to, amount)
    //     - transferFeePreTx(from, amount)
    //     - transferFeePostTx(to, refund, actualUsed)
    //
    //   Gas costs for unchanged operations use standard variable gas accounting.
    //   The four transfer-family functions and approve() above use FIXED_TRANSFER_GAS.


}
