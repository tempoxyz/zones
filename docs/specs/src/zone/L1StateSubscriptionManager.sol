// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { IZoneToken } from "./IZone.sol";

/// @title L1StateSubscriptionManager
/// @notice Zone-side predeploy for managing L1 state subscriptions
/// @dev Deployed at 0x1c00000000000000000000000000000000000001
///      Users subscribe to specific L1 (account, slot) pairs to enable reading that state.
///      TIP-403 policy state for the zone token is automatically subscribed at genesis.
///      Sequencer sets subscription fees and can update them.
contract L1StateSubscriptionManager {
    /*//////////////////////////////////////////////////////////////
                               STORAGE
    //////////////////////////////////////////////////////////////*/

    /// @notice The zone token used to pay subscription fees
    IZoneToken public immutable zoneToken;

    /// @notice Current sequencer address
    address public sequencer;

    /// @notice Pending sequencer for two-step transfer
    address public pendingSequencer;

    /// @notice Daily subscription fee per (account, slot) pair in zone token units
    /// @dev Set by sequencer to cover costs of maintaining L1 state sync
    uint128 public dailySubscriptionFee;

    /// @notice Subscription expiry per (account, slot) pair
    /// @dev Mapping: keccak256(abi.encode(account, slot)) => expiryTimestamp
    ///      0 means never subscribed, type(uint64).max means permanent (auto-subscribed)
    mapping(bytes32 => uint64) public subscriptionExpiry;

    /// @notice TIP-403 registry address on L1 (for auto-subscription)
    address public immutable tip403Registry;

    /// @notice Zone token address on L1 (same as zoneToken address)
    address public immutable l1ZoneToken;

    /*//////////////////////////////////////////////////////////////
                               EVENTS
    //////////////////////////////////////////////////////////////*/

    event SubscriptionCreated(address indexed account, bytes32 indexed slot, uint64 expiryTimestamp);
    event SubscriptionExtended(address indexed account, bytes32 indexed slot, uint64 newExpiryTimestamp);
    event DailyFeeUpdated(uint128 newFee);
    event SequencerTransferStarted(address indexed currentSequencer, address indexed pendingSequencer);
    event SequencerTransferred(address indexed previousSequencer, address indexed newSequencer);

    /*//////////////////////////////////////////////////////////////
                               ERRORS
    //////////////////////////////////////////////////////////////*/

    error OnlySequencer();
    error NotPendingSequencer();
    error SubscriptionExpired();
    error InsufficientPayment();
    error PermanentSubscription();

    /*//////////////////////////////////////////////////////////////
                            CONSTRUCTOR
    //////////////////////////////////////////////////////////////*/

    /// @notice Initialize with sequencer and auto-subscribe TIP-403 policy state
    /// @param _zoneToken The zone token address
    /// @param _sequencer The sequencer address
    /// @param _tip403Registry TIP-403 registry address on L1
    /// @param _l1ZoneToken Zone token address on L1 (for reading transferPolicyId)
    constructor(
        address _zoneToken,
        address _sequencer,
        address _tip403Registry,
        address _l1ZoneToken
    ) {
        zoneToken = IZoneToken(_zoneToken);
        sequencer = _sequencer;
        tip403Registry = _tip403Registry;
        l1ZoneToken = _l1ZoneToken;

        // Auto-subscribe to TIP-403 policy state (permanent subscriptions)
        // These are required for TIP-20 transfer inference to work

        // 1. Subscribe to zone token's transferPolicyId: _transferPolicyId slot in TIP-20
        //    Storage layout: slot 0
        bytes32 transferPolicySlot = bytes32(uint256(0));
        bytes32 subKey1 = keccak256(abi.encode(_l1ZoneToken, transferPolicySlot));
        subscriptionExpiry[subKey1] = type(uint64).max;
        emit SubscriptionCreated(_l1ZoneToken, transferPolicySlot, type(uint64).max);

        // 2. Subscribe to TIP-403 policy data: _policyData[transferPolicyId]
        //    Note: We can't subscribe to specific policy ID at construction since we don't know it yet.
        //    The sequencer must call autoSubscribePolicyState() after reading the actual transferPolicyId.

        // 3. Subscribe to TIP-403 policy set for any address: policySet[transferPolicyId][address]
        //    Note: This is handled dynamically - see autoSubscribePolicyState()
    }

    /*//////////////////////////////////////////////////////////////
                       SEQUENCER MANAGEMENT
    //////////////////////////////////////////////////////////////*/

    /// @notice Start a sequencer transfer. Only callable by current sequencer.
    function transferSequencer(address newSequencer) external {
        if (msg.sender != sequencer) revert OnlySequencer();
        pendingSequencer = newSequencer;
        emit SequencerTransferStarted(sequencer, newSequencer);
    }

    /// @notice Accept a pending sequencer transfer. Only callable by pending sequencer.
    function acceptSequencer() external {
        if (msg.sender != pendingSequencer) revert NotPendingSequencer();
        address previousSequencer = sequencer;
        sequencer = pendingSequencer;
        pendingSequencer = address(0);
        emit SequencerTransferred(previousSequencer, sequencer);
    }

    /*//////////////////////////////////////////////////////////////
                          FEE MANAGEMENT
    //////////////////////////////////////////////////////////////*/

    /// @notice Set daily subscription fee. Only callable by sequencer.
    /// @param newFee New daily fee in zone token units
    function setDailyFee(uint128 newFee) external {
        if (msg.sender != sequencer) revert OnlySequencer();
        dailySubscriptionFee = newFee;
        emit DailyFeeUpdated(newFee);
    }

    /*//////////////////////////////////////////////////////////////
                        SUBSCRIPTION MANAGEMENT
    //////////////////////////////////////////////////////////////*/

    /// @notice Subscribe to an L1 state slot for N days
    /// @dev Transfers zone tokens equal to dailySubscriptionFee * days to sequencer
    ///      Extends existing subscription if already subscribed
    /// @param account The L1 contract address
    /// @param slot The storage slot
    /// @param days Number of days to subscribe (1-3650, ~10 years max)
    function subscribe(address account, bytes32 slot, uint16 days) external {
        if (days == 0 || days > 3650) revert("Invalid days");

        bytes32 subKey = keccak256(abi.encode(account, slot));

        // Check if this is a permanent (auto-subscribed) slot
        if (subscriptionExpiry[subKey] == type(uint64).max) {
            revert PermanentSubscription();
        }

        // Calculate payment
        uint128 payment = dailySubscriptionFee * uint128(days);
        if (payment < dailySubscriptionFee) revert("Overflow"); // Basic overflow check

        // Transfer tokens to sequencer
        if (!zoneToken.transferFrom(msg.sender, sequencer, payment)) {
            revert InsufficientPayment();
        }

        // Calculate new expiry
        uint64 currentExpiry = subscriptionExpiry[subKey];
        uint64 baseTime = currentExpiry > block.timestamp ? currentExpiry : uint64(block.timestamp);
        uint64 extension = uint64(days) * 1 days;
        uint64 newExpiry = baseTime + extension;

        subscriptionExpiry[subKey] = newExpiry;

        if (currentExpiry == 0) {
            emit SubscriptionCreated(account, slot, newExpiry);
        } else {
            emit SubscriptionExtended(account, slot, newExpiry);
        }
    }

    /// @notice Auto-subscribe to TIP-403 policy state for the zone token
    /// @dev Called by sequencer after reading transferPolicyId from L1.
    ///      Subscribes to policy data and creates a permanent subscription template for policy sets.
    /// @param transferPolicyId The zone token's transfer policy ID from L1
    function autoSubscribePolicyState(uint256 transferPolicyId) external {
        if (msg.sender != sequencer) revert OnlySequencer();

        // Subscribe to policy data: _policyData[transferPolicyId] in TIP403Registry
        // Storage slot = keccak256(abi.encode(transferPolicyId, uint256(1)))
        bytes32 policyDataSlot = keccak256(abi.encode(transferPolicyId, uint256(1)));
        bytes32 subKey = keccak256(abi.encode(tip403Registry, policyDataSlot));

        if (subscriptionExpiry[subKey] == 0) {
            subscriptionExpiry[subKey] = type(uint64).max;
            emit SubscriptionCreated(tip403Registry, policyDataSlot, type(uint64).max);
        }

        // Note: Policy set subscriptions policySet[transferPolicyId][address] are handled
        // dynamically by the sequencer when it sees new addresses in TIP-20 transfers.
        // The sequencer automatically subscribes to policy set entries as needed.
    }

    /// @notice Check if a subscription is active
    /// @param account The L1 contract address
    /// @param slot The storage slot
    /// @return active True if subscription is active (not expired)
    function isSubscribed(address account, bytes32 slot) external view returns (bool active) {
        bytes32 subKey = keccak256(abi.encode(account, slot));
        uint64 expiry = subscriptionExpiry[subKey];

        if (expiry == type(uint64).max) {
            return true; // Permanent subscription
        }

        return expiry > block.timestamp;
    }

    /// @notice Get subscription expiry timestamp
    /// @param account The L1 contract address
    /// @param slot The storage slot
    /// @return expiry Expiry timestamp (0 if never subscribed, max uint64 if permanent)
    function getSubscriptionExpiry(address account, bytes32 slot) external view returns (uint64 expiry) {
        bytes32 subKey = keccak256(abi.encode(account, slot));
        return subscriptionExpiry[subKey];
    }
}
